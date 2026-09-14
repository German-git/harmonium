use super::runner::{
    EffectGuard, dispatch_with_guard, dispatch_with_operation, execute_effects, worker_panic_error,
};
use super::*;
use crate::audio::{AudioOutput, OutputProvider};
use crate::runtime::{
    AppServices, AudioSink, EffectServices, OperationKind, OperationRegistrationError, Spawner,
};
use crate::state::StreamActivity;
use crate::test_support::{TestTempDir, unique_temp_dir};
use crossterm::event::{KeyCode, KeyModifiers};
use std::fs;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{RecvTimeoutError, Sender};
use std::sync::{Arc, Barrier, Mutex};
use std::time::Duration;

/// Generous ceiling so slow machines never turn timing into flakes.
const EVENT_WAIT: Duration = Duration::from_secs(5);

/// Build a fresh in-flight effect counter for tests that exercise
/// [`execute_effects`].
///
/// Each call returns a brand-new counter so two effects in the same
/// test do not pollute each other's counts, and the lifetime is tied to
/// the test body so the `Arc` outlives every spawned task it owns.
fn pending_counter() -> std::sync::Arc<AtomicUsize> {
    std::sync::Arc::new(AtomicUsize::new(0))
}

#[derive(Debug, Default)]
struct RecordingAudioSink {
    commands: Mutex<Vec<crate::audio::AudioCommand>>,
}

impl AudioSink for RecordingAudioSink {
    fn send_audio(
        &self,
        command: crate::audio::AudioCommand,
    ) -> Result<(), std::sync::mpsc::SendError<crate::audio::AudioCommand>> {
        self.commands
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .push(command);
        Ok(())
    }
}

struct FakeEffectServices {
    events: crate::event::EventBus,
    audio: RecordingAudioSink,
    lyrics: crate::lyrics::LyricsService,
    resolver: crate::stream::StreamResolver,
    theme_repository: Arc<dyn crate::ui::theme::ThemeRepository>,
    config_writer: Arc<crate::runtime::ConfigWriteCoordinator>,
    runtime_state_writer: Arc<crate::runtime::RuntimeStateWriteCoordinator>,
}

impl FakeEffectServices {
    fn new() -> Self {
        Self {
            events: crate::event::EventBus::new(),
            audio: RecordingAudioSink::default(),
            lyrics: crate::lyrics::LyricsService::new(),
            resolver: crate::stream::resolver_with_defaults(),
            theme_repository: Arc::new(crate::ui::theme::FileThemeRepository),
            config_writer: Arc::new(crate::runtime::ConfigWriteCoordinator::default()),
            runtime_state_writer: Arc::new(crate::runtime::RuntimeStateWriteCoordinator::default()),
        }
    }
}

impl Spawner for FakeEffectServices {
    fn spawn_operation(
        &self,
        _name: &'static str,
        _kind: crate::runtime::OperationKind,
        _task: crate::runtime::OperationTask,
    ) -> Result<crate::runtime::OperationHandle, crate::runtime::OperationRegistrationError> {
        Err(crate::runtime::OperationRegistrationError::Closed)
    }

    fn spawn_background(
        &self,
        _name: &'static str,
        _kind: crate::event::EffectErrorKind,
        _task: crate::runtime::SpawnFuture,
    ) -> Result<(), crate::runtime::OperationRegistrationError> {
        Err(crate::runtime::OperationRegistrationError::Closed)
    }

    fn spawn_background_with_context(
        &self,
        _name: &'static str,
        _kind: crate::event::EffectErrorKind,
        _task: crate::runtime::OperationTask,
    ) -> Result<crate::runtime::OperationHandle, crate::runtime::OperationRegistrationError> {
        Err(crate::runtime::OperationRegistrationError::Closed)
    }
}

impl EffectServices for FakeEffectServices {
    fn event_sender(&self) -> crate::event::EventSender {
        self.events.sender()
    }

    fn audio_sink(&self) -> &dyn AudioSink {
        &self.audio
    }

    fn artwork_loader(&self) -> Option<&crate::artwork::ArtworkLoader> {
        None
    }

    fn lyrics_service(&self) -> &crate::lyrics::LyricsService {
        &self.lyrics
    }

    fn playlist_store(&self) -> Option<Arc<dyn crate::playlist::PlaylistRepository>> {
        None
    }

    fn stream_resolver(&self) -> &crate::stream::StreamResolver {
        &self.resolver
    }

    fn config_writer(&self) -> Arc<crate::runtime::ConfigWriteCoordinator> {
        Arc::clone(&self.config_writer)
    }

    fn runtime_state_writer(&self) -> Arc<crate::runtime::RuntimeStateWriteCoordinator> {
        Arc::clone(&self.runtime_state_writer)
    }

    fn theme_repository(&self) -> Arc<dyn crate::ui::theme::ThemeRepository> {
        Arc::clone(&self.theme_repository)
    }

    fn set_lyrics_remote_enabled(&self, enabled: bool) {
        self.lyrics.set_remote_enabled(enabled);
    }
}

#[test]
fn execute_effect_uses_injected_services_without_app_services() {
    let services = FakeEffectServices::new();
    let mut app = App::new();
    let effects = vec![Effect::Audio(crate::audio::AudioCommand::SetVolume(
        volume(42),
    ))];

    app.execute_effects(effects, &services);

    let commands = services
        .audio
        .commands
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    assert!(
        matches!(commands.as_slice(), [crate::audio::AudioCommand::SetVolume(value)] if *value == volume(42))
    );
}

fn playlist_name(name: &str) -> crate::playlist::PlaylistName {
    crate::playlist::PlaylistName::try_from(name).expect("valid playlist name")
}

fn volume(value: u16) -> crate::audio::VolumePercent {
    crate::audio::VolumePercent::new(value).expect("test volume must be valid")
}

fn gain(value: f32) -> crate::audio::GainDb {
    crate::audio::GainDb::try_from(value).expect("test gain must be valid")
}

fn crossfade(value: u16) -> crate::audio::CrossfadeSeconds {
    crate::audio::CrossfadeSeconds::new(value).expect("test crossfade must be valid")
}

fn key(value: &str) -> crate::input::KeyChord {
    crate::input::KeyChord::try_from(value).expect("test key must be valid")
}

#[test]
fn capability_queue_mutation_preserves_selection_viewport_invariant() {
    let mut app = App::new();
    app.set_playlist_viewport_height(1);
    app.extend_playlist([
        PathBuf::from("/music/one.mp3"),
        PathBuf::from("/music/two.mp3"),
    ]);

    app.select_playlist_track(1);

    assert_eq!(app.playlist().cursor(), 1);
    assert_eq!(app.playlist_scroll_offset(), 1);
}

#[test]
fn startup_capability_applies_preferences_and_resume_identity() {
    let mut app = App::new();
    let mut config = AppConfig::default();
    config.general.confirm_quit = false;
    config.general.show_hidden = true;
    config.general.volume_percent = volume(73);
    let persisted = PersistedState {
        repeat_mode: crate::playback_mode::RepeatMode::All,
        shuffle: true,
        last_track_path: Some("/music/saved.mp3".to_string()),
        last_track_position_ms: 12_000,
        ..PersistedState::default()
    };

    app.apply_startup_preferences(&config, &persisted);

    assert!(!app.state().confirm_quit);
    assert!(app.state().browser.show_hidden);
    assert_eq!(app.playback().volume_percent, volume(73));
    assert_eq!(
        app.state().playback_mode.repeat(),
        crate::playback_mode::RepeatMode::All
    );
    assert!(app.state().playback_mode.shuffle());
    assert_eq!(
        app.state().persistence.last_track,
        Some(TrackLocation::local("/music/saved.mp3"))
    );
}

#[derive(Debug)]
struct DelayedOutputProvider {
    delay: Duration,
    outputs: Vec<AudioOutput>,
}

impl OutputProvider for DelayedOutputProvider {
    fn list_outputs(&self) -> Vec<AudioOutput> {
        std::thread::sleep(self.delay);
        self.outputs.clone()
    }

    fn default_output_id(&self) -> Option<String> {
        None
    }

    fn accept_selection(&self, id: &str) -> anyhow::Result<()> {
        if self
            .outputs
            .iter()
            .any(|output| output.id == id && output.available)
        {
            Ok(())
        } else {
            Err(anyhow::anyhow!("output is not available: {id}"))
        }
    }
}

#[derive(Debug)]
struct DisappearingOutputProvider {
    output: AudioOutput,
}

impl OutputProvider for DisappearingOutputProvider {
    fn cached_outputs(&self) -> Option<Vec<AudioOutput>> {
        Some(vec![AudioOutput::session_default(), self.output.clone()])
    }

    fn list_outputs(&self) -> Vec<AudioOutput> {
        vec![AudioOutput::session_default(), self.output.clone()]
    }

    fn default_output_id(&self) -> Option<String> {
        None
    }

    fn accept_selection(&self, id: &str) -> anyhow::Result<()> {
        if id.is_empty() {
            Ok(())
        } else {
            Err(anyhow::anyhow!("output disappeared: {id}"))
        }
    }
}

#[derive(Debug)]
struct SequencedOutputProvider {
    started: Mutex<Option<Sender<()>>>,
    release_first: Arc<Barrier>,
    calls: AtomicUsize,
    first: Vec<AudioOutput>,
    second: Vec<AudioOutput>,
}

impl OutputProvider for SequencedOutputProvider {
    fn list_outputs(&self) -> Vec<AudioOutput> {
        if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            if let Some(sender) = self.started.lock().expect("started lock").take() {
                sender.send(()).expect("enumeration started receiver");
            }
            self.release_first.wait();
            self.first.clone()
        } else {
            self.second.clone()
        }
    }

    fn default_output_id(&self) -> Option<String> {
        None
    }

    fn accept_selection(&self, id: &str) -> anyhow::Result<()> {
        if self
            .second
            .iter()
            .any(|output| output.id == id && output.available)
        {
            Ok(())
        } else {
            Err(anyhow::anyhow!("output is not available: {id}"))
        }
    }
}

#[test]
fn effect_guard_increments_on_creation_and_decrements_on_drop() {
    let counter = pending_counter();
    assert_eq!(counter.load(std::sync::atomic::Ordering::Relaxed), 0);

    let guard = EffectGuard::new(&counter);
    assert_eq!(
        counter.load(std::sync::atomic::Ordering::Relaxed),
        1,
        "creating a guard bumps the in-flight counter"
    );

    drop(guard);
    assert_eq!(
        counter.load(std::sync::atomic::Ordering::Relaxed),
        0,
        "dropping the guard decrements the counter"
    );
}

#[test]
fn effect_guard_decrements_even_when_dropped_via_panic() {
    // The counter must decrement on every unwinding path, not only on a
    // well-behaved drop. A panicking closure that owns the guard still
    // restores the counter when its stack unwinds.
    let counter = pending_counter();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _guard = EffectGuard::new(&counter);
        assert_eq!(
            counter.load(std::sync::atomic::Ordering::Relaxed),
            1,
            "guard increments inside the panicking scope"
        );
        panic!("simulated worker crash");
    }));
    assert!(result.is_err(), "the panic must propagate to the caller");
    assert_eq!(
        counter.load(std::sync::atomic::Ordering::Relaxed),
        0,
        "guard drop during unwind must decrement the counter"
    );
}

#[test]
fn dispatch_with_guard_increments_and_decrements_through_a_task() {
    // Drive the dispatch helper end-to-end: increment, run the inner
    // task, decrement. The counter must return to zero once the spawned
    // task finishes, even though we cannot await it directly from a
    // unit test (the runtime already runs in AppServices::new).
    let services = AppServices::new().expect("services construction");
    let counter = pending_counter();
    let completed = Arc::new(std::sync::Mutex::new(false));
    let completed_inside = Arc::clone(&completed);
    let _ = dispatch_with_guard(
        &services,
        &counter,
        "test-counter",
        EffectErrorKind::Task,
        async move {
            *completed_inside.lock().expect("lock") = true;
            Ok(())
        },
    );

    // Poll until the task finishes so we never race the worker thread.
    let deadline = std::time::Instant::now() + EVENT_WAIT;
    while std::time::Instant::now() < deadline {
        if *completed.lock().expect("lock") {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    assert!(
        *completed.lock().expect("lock"),
        "the dispatched task must run to completion"
    );
    // The guard drops when the spawned future finishes; poll briefly
    // for the counter to settle.
    let deadline = std::time::Instant::now() + EVENT_WAIT;
    while std::time::Instant::now() < deadline
        && counter.load(std::sync::atomic::Ordering::Relaxed) != 0
    {
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    assert_eq!(
        counter.load(std::sync::atomic::Ordering::Relaxed),
        0,
        "dispatch_with_guard must decrement the counter after the task ends"
    );

    services.shutdown();
}

#[test]
fn operation_capacity_failure_does_not_block_on_full_critical_bus() {
    let services = AppServices::new().expect("services construction");
    let mut registered = 0;
    loop {
        match services.spawn_background("held-operation", EffectErrorKind::Task, async {
            std::future::pending::<anyhow::Result<()>>().await
        }) {
            Ok(()) => registered += 1,
            Err(OperationRegistrationError::CapacityExhausted) => break,
            Err(error) => panic!("unexpected operation registration error: {error}"),
        }
    }
    assert!(registered > 0, "the registry must accept held operations");

    for track_index in 0..256 {
        services
            .events()
            .send_critical(AppEvent::TrackEnded { track_index })
            .expect("critical queue should accept its fixed capacity");
    }
    assert_eq!(
        services.events().send(AppEvent::Tick),
        Err(crate::event::EventSendError::Full),
        "the regression must exercise a queue full of critical events"
    );

    let started = std::time::Instant::now();
    let result = dispatch_with_operation(
        &services,
        &pending_counter(),
        "capacity-regression",
        OperationKind::Search,
        |_| async { Ok(()) },
    );

    assert!(
        started.elapsed() < Duration::from_millis(250),
        "capacity failure must not wait for the UI event loop: {:?}",
        started.elapsed()
    );
    assert!(
        result.is_ok(),
        "a full generic partition must not reject an independent search operation"
    );

    services.shutdown();
}

#[test]
fn add_stream_dispatch_sets_stream_activity() {
    let mut app = App::new();
    // Open the Add Stream popup so handle_dialog_confirm can route to
    // the stream branch.
    app.handle_command(Command::AddStream);
    app.state_mut()
        .popup_dialog
        .set_dialog_input("https://example.com/live".to_string());
    assert!(
        app.state().async_ops.stream_activity().is_none(),
        "flag must start cleared"
    );

    let effects = app.handle_command(Command::ConfirmDialog);
    assert!(
        effects
            .iter()
            .any(|e| matches!(e, Effect::ResolveStream { .. })),
        "submitting a URL must dispatch ResolveStream"
    );
    assert!(
        app.state().async_ops.stream_activity().is_some(),
        "the Now Playing band must observe the in-flight flag"
    );
}

#[test]
fn failed_stream_dispatch_clears_only_its_current_activity() {
    let mut app = App::new();
    let (request_id, _) = app.state_mut().async_ops.begin_stream_resolution();

    app.compensate_dispatch_failure(runner::DispatchFailure {
        operation: "stream-resolve",
        error: OperationRegistrationError::CapacityExhausted,
        compensation: runner::DispatchCompensation::StreamResolution { request_id },
    });

    assert!(
        app.state().async_ops.stream_activity().is_none(),
        "failed dispatch must leave the stream UI out of its loading state"
    );
    assert!(
        app.state()
            .notifications
            .last()
            .is_some_and(|message| message.contains("UI state rolled back"))
    );
}

#[test]
fn stream_resolved_clears_stream_activity_on_both_outcomes() {
    let mut app = App::new();
    let url = url::Url::parse("https://example.com/live").expect("valid url");
    // Open the popup so apply_stream_resolved has a dialog to clear;
    // apply_stream_resolved must always clear the flag regardless.
    app.state_mut().popup_dialog.open_dialog(
        DialogMode::AddStream {
            error: None,
            loading: true,
        },
        String::new(),
        None,
    );
    let (first_request, _) = app.state_mut().begin_stream_resolution();

    // Success path: a track arrives.
    app.apply_stream_resolved(first_request, url.clone(), None, None);
    assert!(
        app.state().async_ops.stream_activity().is_none(),
        "the success path must clear stream activity"
    );

    // Failure path: the resolver reports an error.
    let (second_request, _) = app.state_mut().begin_stream_resolution();
    app.apply_stream_resolved(
        second_request,
        url,
        None,
        Some(WorkerError::message("stream-resolve", "boom")),
    );
    assert!(
        app.state().async_ops.stream_activity().is_none(),
        "the failure path must also clear stream activity"
    );
}

#[test]
fn cancelled_stream_resolution_cannot_append_a_late_result() {
    let mut app = App::new();
    app.handle_command(Command::AddStream);
    app.state_mut()
        .popup_dialog
        .set_dialog_input("https://example.com/old".to_string());
    let effects = app.handle_command(Command::ConfirmDialog);
    let request_id = effects
        .iter()
        .find_map(|effect| match effect {
            Effect::ResolveStream { request_id, .. } => Some(*request_id),
            _ => None,
        })
        .expect("stream submission creates a request");

    app.handle_key_event(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    assert!(app.state().async_ops.stream_activity().is_none());
    assert!(app.state().popup_dialog.active_popup_ref().is_none());

    let track = crate::track::Track::from_stream(
        url::Url::parse("https://example.com/old").expect("valid url"),
        crate::stream::StreamKind::Http,
    );
    app.apply_stream_resolved(
        request_id,
        url::Url::parse("https://example.com/old").expect("valid url"),
        Some(Box::new(track)),
        None,
    );
    assert_eq!(app.state().playlist.len(), 0);
    assert!(app.state().async_ops.stream_activity().is_none());
}

#[test]
fn newer_stream_resolution_rejects_the_previous_result_without_clearing_loading() {
    let mut app = App::new();
    app.handle_command(Command::AddStream);

    app.state_mut()
        .popup_dialog
        .set_dialog_input("https://example.com/first".to_string());
    let first = app.handle_command(Command::ConfirmDialog);
    let first_id = first
        .iter()
        .find_map(|effect| match effect {
            Effect::ResolveStream { request_id, .. } => Some(*request_id),
            _ => None,
        })
        .expect("first submission creates a request");

    app.state_mut()
        .popup_dialog
        .set_dialog_input("https://example.com/second".to_string());
    let second = app.handle_command(Command::ConfirmDialog);
    let second_id = second
        .iter()
        .find_map(|effect| match effect {
            Effect::ResolveStream { request_id, .. } => Some(*request_id),
            _ => None,
        })
        .expect("second submission creates a request");
    assert_ne!(first_id, second_id);

    let first_track = crate::track::Track::from_stream(
        url::Url::parse("https://example.com/first").expect("valid url"),
        crate::stream::StreamKind::Http,
    );
    app.apply_stream_resolved(
        first_id,
        url::Url::parse("https://example.com/first").expect("valid url"),
        Some(Box::new(first_track)),
        None,
    );
    assert_eq!(app.state().playlist.len(), 0);
    assert!(app.state().async_ops.stream_activity().is_some());
    assert_eq!(
        match app.state().async_ops.stream_activity() {
            Some(StreamActivity::Resolving { request_id, .. }) => Some(*request_id),
            _ => None,
        },
        Some(second_id)
    );
}

#[test]
fn source_ready_clears_stream_activity_for_the_current_track() {
    let mut app = App::new();
    let url = url::Url::parse("https://example.com/live").expect("valid url");
    let track = crate::track::Track::from_stream(url.clone(), crate::stream::StreamKind::Http);
    {
        let state = app.state_mut();
        state.playlist = {
            let mut pl = crate::playlist::Playlist::new();
            pl.extend([track]);
            pl
        };
        state.playlist.select(0);
    }
    let _ = app.begin_current_track();
    let generation = app
        .state()
        .async_ops
        .stream_activity()
        .and_then(|activity| match activity {
            StreamActivity::Acquiring { generation, .. } => Some(*generation),
            StreamActivity::Resolving { .. } => None,
        })
        .expect("stream start has a generation");

    app.apply_source_ready_with_generation(generation, url.to_string());
    assert!(
        app.state().async_ops.stream_activity().is_none(),
        "the SourceReady handler must drop the flag once decoding finishes"
    );
}

#[test]
fn source_failed_clears_stream_activity_only_for_the_active_stream() {
    let mut app = App::new();
    let first_url = url::Url::parse("https://example.com/first").expect("valid url");
    let second_url = url::Url::parse("https://example.com/second").expect("valid url");
    {
        let state = app.state_mut();
        state.playlist = {
            let mut playlist = crate::playlist::Playlist::new();
            playlist.extend([
                crate::track::Track::from_stream(
                    first_url.clone(),
                    crate::stream::StreamKind::Http,
                ),
                crate::track::Track::from_stream(
                    second_url.clone(),
                    crate::stream::StreamKind::Http,
                ),
            ]);
            playlist
        };
        state.playlist.select(0);
    }

    let _ = app.begin_current_track();
    app.state_mut().playlist.select(1);
    let _ = app.begin_current_track();

    // The failed first request publishes a stopped snapshot before its
    // SourceFailed event. The snapshot must not erase the second request's
    // identity before the failure handler validates it.
    app.apply_playback_progress(crate::audio::PlaybackSnapshot {
        status: PlayStatus::Stopped,
        track_index: Some(0),
        elapsed: Duration::ZERO,
        duration: None,
        sink_health: crate::audio::SinkHealth::Healthy,
    });
    assert_eq!(
        app.state().playback.track_index,
        Some(1),
        "a stale failure snapshot must be reanchored to the newer stream"
    );
    app.apply_source_failed(first_url.to_string());
    assert!(
        app.state().async_ops.stream_activity().is_some(),
        "a stale failure must not clear a newer stream request's spinner"
    );

    app.apply_source_failed(second_url.to_string());
    assert!(
        app.state().async_ops.stream_activity().is_none(),
        "a matching acquisition failure must clear the stream spinner"
    );
}

#[test]
fn contextual_search_enters_loading_and_drops_stale_completion() {
    let mut app = App::new();
    app.state_mut().active_panel = Panel::Browser;

    let effects = app.handle_key_event(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE));
    assert!(effects.is_empty());
    assert!(matches!(
        app.active_popup(),
        Some(Popup::SearchQuery {
            scope: SearchScope::Browser
        })
    ));

    app.state_mut()
        .popup_dialog
        .set_search_query("flow*.flac".to_string());
    let effects = app.handle_key_event(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert!(matches!(
        effects.as_slice(),
        [Effect::Search {
            request_id: 1,
            scope: SearchScope::Browser,
            ..
        }]
    ));
    assert!(matches!(
        app.active_popup(),
        Some(Popup::SearchLoading {
            request_id: 1,
            scope: SearchScope::Browser
        })
    ));

    app.apply_search_completed(
        2,
        SearchScope::Browser,
        vec![SearchResult {
            identity: TrackLocation::local("/music/new.flac"),
            label: "new.flac".to_string(),
        }],
        None,
    );
    assert!(matches!(
        app.active_popup(),
        Some(Popup::SearchLoading { request_id: 1, .. })
    ));
}

#[test]
fn owned_search_effect_publishes_completion() {
    let root = unique_temp_dir("owned-search-effect");
    fs::write(root.join("song.flac"), b"fixture").expect("search fixture");
    let services = AppServices::new().expect("services construction");

    execute_effects(
        vec![Effect::Search {
            request_id: 7,
            scope: SearchScope::Browser,
            root: root.to_path_buf(),
            query: "song".to_string(),
            tracks: Vec::new(),
        }],
        &services,
        &pending_counter(),
    );

    let AppEvent::SearchCompleted {
        operation_id,
        request_id,
        scope,
        results,
        message,
    } = services
        .events()
        .recv_timeout(EVENT_WAIT)
        .expect("search completion")
    else {
        panic!("expected SearchCompleted event");
    };
    assert_eq!(request_id, 7);
    assert_eq!(scope, SearchScope::Browser);
    assert_eq!(message, None);
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].label, "song.flac");
    services.release_operation_event(Some(operation_id));
    services.shutdown();
}

#[test]
fn playlist_search_reveals_the_selected_track_by_location() {
    let mut app = App::new();
    app.state_mut().active_panel = Panel::Playlist;
    app.state_mut().playlist.extend([
        crate::track::Track::local("/music/first.flac"),
        crate::track::Track::local("/music/second.flac"),
    ]);

    app.handle_command(Command::OpenSearch);
    app.state_mut()
        .popup_dialog
        .set_search_query("flac".to_string());
    let effects = app.handle_key_event(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert!(matches!(effects.first(), Some(Effect::Search { .. })));
    app.apply_search_completed(
        1,
        SearchScope::Playlist,
        vec![
            SearchResult {
                identity: TrackLocation::local("/music/first.flac"),
                label: "first".to_string(),
            },
            SearchResult {
                identity: TrackLocation::local("/music/second.flac"),
                label: "second".to_string(),
            },
        ],
        None,
    );

    app.handle_key_event(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE));
    let effects = app.handle_key_event(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    complete_playlist_effects(&mut app, effects);

    assert_eq!(app.state().playlist.cursor(), 1);
    assert_eq!(app.active_panel(), Panel::Playlist);
    assert!(app.active_popup().is_none());
}

#[test]
fn search_results_navigation_uses_vertical_keys_only() {
    let mut app = App::new();
    app.state_mut()
        .popup_dialog
        .open_search(SearchScope::Playlist);
    app.state_mut()
        .popup_dialog
        .replace_popup(Popup::SearchResults {
            scope: SearchScope::Playlist,
            request_id: 1,
            results: vec![
                SearchResult {
                    identity: TrackLocation::local("/music/first.flac"),
                    label: "first".to_string(),
                },
                SearchResult {
                    identity: TrackLocation::local("/music/second.flac"),
                    label: "second".to_string(),
                },
                SearchResult {
                    identity: TrackLocation::local("/music/third.flac"),
                    label: "third".to_string(),
                },
            ],
            cursor: 1,
        });

    app.handle_key_event(KeyEvent::new(KeyCode::Char('h'), KeyModifiers::NONE));
    assert!(matches!(
        app.active_popup(),
        Some(Popup::SearchResults { cursor: 1, .. })
    ));

    app.handle_key_event(KeyEvent::new(KeyCode::Char('k'), KeyModifiers::NONE));
    assert!(matches!(
        app.active_popup(),
        Some(Popup::SearchResults { cursor: 0, .. })
    ));

    app.handle_key_event(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE));
    assert!(matches!(
        app.active_popup(),
        Some(Popup::SearchResults { cursor: 1, .. })
    ));

    app.handle_key_event(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
    assert!(matches!(
        app.active_popup(),
        Some(Popup::SearchResults { cursor: 2, .. })
    ));

    app.handle_key_event(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
    assert!(matches!(
        app.active_popup(),
        Some(Popup::SearchResults { cursor: 1, .. })
    ));
}

#[test]
fn browser_search_reveals_a_file_in_its_parent_directory() {
    let root = unique_temp_dir("search-reveal");
    fs::write(root.join("song.flac"), "").expect("song");
    let mut app = App::new();
    app.state_mut().active_panel = Panel::Browser;
    app.state_mut().change_browser_dir(
        root.to_path_buf(),
        crate::filesystem::read_sorted_entries(&root, false).expect("browser listing"),
    );
    app.state_mut()
        .popup_dialog
        .open_search(SearchScope::Browser);
    app.state_mut()
        .popup_dialog
        .replace_popup(Popup::SearchResults {
            scope: SearchScope::Browser,
            request_id: 1,
            results: vec![SearchResult {
                identity: TrackLocation::local(root.join("song.flac")),
                label: "song.flac".to_string(),
            }],
            cursor: 0,
        });

    let effects = app.handle_key_event(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    complete_playlist_effects(&mut app, effects);

    assert_eq!(app.state().browser.current_dir, root.path());
    assert_eq!(
        app.state().browser.entries[app.state().browser.cursor()].name,
        "song.flac"
    );
}

#[test]
fn stale_browser_directory_results_do_not_replace_the_latest_listing() {
    let mut app = App::new();
    let first = app.request_browser_dir(PathBuf::from("/first"));
    let second = app.request_browser_dir(PathBuf::from("/second"));

    let Effect::LoadBrowserDirectory {
        request_id: first_id,
        dir: first_dir,
        restore_cursor_name: first_restore,
        ..
    } = first
    else {
        panic!("expected the first browser request");
    };
    let Effect::LoadBrowserDirectory {
        request_id: second_id,
        dir: second_dir,
        restore_cursor_name: second_restore,
        ..
    } = second
    else {
        panic!("expected the second browser request");
    };

    let second_entries = vec![crate::filesystem::FileEntry::new(
        "new.mp3",
        PathBuf::from("/second/new.mp3"),
        crate::filesystem::EntryKind::File,
    )];
    let _ =
        app.apply_browser_directory_loaded(second_id, second_dir, second_entries, second_restore);

    let first_entries = vec![crate::filesystem::FileEntry::new(
        "old.mp3",
        PathBuf::from("/first/old.mp3"),
        crate::filesystem::EntryKind::File,
    )];
    assert!(
        app.apply_browser_directory_loaded(first_id, first_dir, first_entries, first_restore,)
            .is_empty()
    );
    assert_eq!(app.state().browser.current_dir, PathBuf::from("/second"));
    assert_eq!(app.state().browser.entries[0].name, "new.mp3");
}

#[test]
fn browser_directory_persists_only_after_the_accepted_listing_arrives() {
    let root = unique_temp_dir("browser-persistence");
    let old_dir = root.path().join("old");
    let new_dir = root.path().join("new");
    fs::create_dir_all(&old_dir).expect("old directory");
    fs::create_dir_all(&new_dir).expect("new directory");
    let config_dir = root.path().join("config");
    let data_dir = root.path().join("data");

    let mut app = App::new();
    app.set_config_paths(AppConfig::default(), config_dir.clone(), data_dir);
    app.state_mut()
        .change_browser_dir(old_dir.clone(), Vec::new());
    let draft = SettingsDraft {
        browser_directory: new_dir.to_string_lossy().into_owned(),
        confirm_quit: true,
        ..SettingsDraft::default()
    };
    app.state_mut().popup_dialog.open_popup(Popup::Settings {
        tab: SettingsTab::General,
        focus: SettingsFocus::Content,
        draft,
    });

    let effects = app.handle_key_event(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    let Effect::LoadBrowserDirectory {
        request_id,
        dir,
        restore_cursor_name,
        ..
    } = effects.into_iter().next().expect("directory effect")
    else {
        panic!("expected a browser directory load effect");
    };

    assert_eq!(app.state().browser.current_dir, old_dir);
    assert!(
        !config_dir.join("config.toml").exists(),
        "the previous directory must not be persisted before enumeration"
    );

    let services = AppServices::new().expect("services construction");
    let save_effects =
        app.apply_browser_directory_loaded(request_id, dir, Vec::new(), restore_cursor_name);
    execute_effects(
        save_effects,
        &services,
        &app.state().async_ops.pending_effects,
    );
    let mut config_saved = false;
    for _ in 0..2 {
        let event = services
            .events()
            .recv_timeout(EVENT_WAIT)
            .expect("persistence completion");
        let operation_id = match event {
            AppEvent::ConfigSaved {
                result: Ok(true),
                operation_id,
                ..
            } => {
                config_saved = true;
                operation_id
            }
            AppEvent::RuntimeStateSaved {
                result: Ok(true),
                operation_id,
                ..
            } => operation_id,
            other => panic!("expected persistence completion, got {other:?}"),
        };
        services.release_operation_event(Some(operation_id));
    }
    assert!(config_saved);
    assert_eq!(app.state().browser.current_dir, new_dir);
    assert_eq!(
        AppConfig::load(&config_dir).general.browser_directory,
        new_dir.to_string_lossy()
    );
    services.shutdown();
}

#[test]
fn settings_close_persists_unrelated_changes_when_browser_load_is_stale() {
    let root = unique_temp_dir("browser-persistence-stale");
    let old_dir = root.path().join("old");
    let requested_dir = root.path().join("requested");
    let newer_dir = root.path().join("newer");
    fs::create_dir_all(&old_dir).expect("old directory");
    fs::create_dir_all(&requested_dir).expect("requested directory");
    fs::create_dir_all(&newer_dir).expect("newer directory");
    let config_dir = root.path().join("config");
    let data_dir = root.path().join("data");

    let mut config = AppConfig::default();
    config.general.browser_directory = old_dir.to_string_lossy().into_owned();
    let mut app = App::new();
    app.set_config_paths(config, config_dir.clone(), data_dir);
    app.state_mut()
        .change_browser_dir(old_dir.clone(), Vec::new());
    let draft = SettingsDraft {
        browser_directory: requested_dir.to_string_lossy().into_owned(),
        confirm_quit: false,
        ..SettingsDraft::default()
    };
    app.state_mut().popup_dialog.open_popup(Popup::Settings {
        tab: SettingsTab::General,
        focus: SettingsFocus::Content,
        draft,
    });

    let effects = app.handle_key_event(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    let Effect::LoadBrowserDirectory {
        request_id,
        dir,
        restore_cursor_name,
        ..
    } = effects
        .iter()
        .find(|effect| matches!(effect, Effect::LoadBrowserDirectory { .. }))
        .expect("directory effect")
    else {
        panic!("expected a browser directory load effect");
    };
    let request_id = *request_id;
    let dir = dir.clone();
    let restore_cursor_name = restore_cursor_name.clone();

    let services = AppServices::new().expect("services construction");
    let save_effects = effects
        .iter()
        .filter(|effect| matches!(effect, Effect::SaveConfig { .. }))
        .cloned()
        .collect::<Vec<_>>();
    execute_effects(
        save_effects,
        &services,
        &app.state().async_ops.pending_effects,
    );
    let AppEvent::ConfigSaved { operation_id, .. } = services
        .events()
        .recv_timeout(EVENT_WAIT)
        .expect("config save completion")
    else {
        panic!("expected config save completion");
    };
    services.release_operation_event(Some(operation_id));
    let saved_after_close = AppConfig::load(&config_dir);
    assert!(!saved_after_close.general.confirm_quit);
    assert_eq!(
        saved_after_close.general.browser_directory,
        old_dir.to_string_lossy()
    );

    // A newer browser request makes the Settings result stale. It must not
    // replace the view or persist the requested directory.
    let _ = app.request_browser_dir(newer_dir);
    assert!(
        app.apply_browser_directory_loaded(request_id, dir, Vec::new(), restore_cursor_name,)
            .is_empty()
    );

    let saved_after_stale = AppConfig::load(&config_dir);
    assert!(!saved_after_stale.general.confirm_quit);
    assert_eq!(
        saved_after_stale.general.browser_directory,
        old_dir.to_string_lossy()
    );
    assert_eq!(app.state().browser.current_dir, old_dir);
    services.shutdown();
}

#[test]
fn config_save_effect_dispatches_and_completes_on_the_worker() {
    let root = unique_temp_dir("config-save-effect");
    let config_dir = root.path().join("config");
    let mut config = AppConfig::default();
    config.general.confirm_quit = false;
    let mut app = App::new();
    app.set_config_paths(config, config_dir.clone(), root.path().join("data"));

    let effect = app.save_config_effect();
    assert!(matches!(effect, Effect::SaveConfig { request_id: 1, .. }));

    let services = AppServices::new().expect("services construction");
    execute_effects(
        vec![effect],
        &services,
        &app.state().async_ops.pending_effects,
    );
    let AppEvent::ConfigSaved {
        operation_id,
        request_id: 1,
        result: Ok(true),
    } = services
        .events()
        .recv_timeout(EVENT_WAIT)
        .expect("config save completion")
    else {
        panic!("expected a config save completion");
    };
    assert_eq!(AppConfig::load(&config_dir).general.confirm_quit, false);
    services.release_operation_event(Some(operation_id));
    services.shutdown();
}

#[test]
fn config_save_failure_reaches_the_ui_as_a_typed_path_scoped_completion() {
    let root = unique_temp_dir("config-save-failure");
    let config_dir = root.path().join("config-path");
    fs::write(&config_dir, "not a directory").expect("blocking config path fixture");
    let config_path = config_dir.join("config.toml");
    let mut app = App::new();
    app.set_config_paths(AppConfig::default(), config_dir, root.path().join("data"));

    let services = AppServices::new().expect("services construction");
    execute_effects(
        vec![app.save_config_effect()],
        &services,
        &app.state().async_ops.pending_effects,
    );

    let AppEvent::ConfigSaved {
        operation_id,
        result: Err(error),
        ..
    } = services
        .events()
        .recv_timeout(EVENT_WAIT)
        .expect("config save failure completion")
    else {
        panic!("expected config save failure completion");
    };
    assert!(
        error
            .to_string()
            .contains(&config_path.display().to_string())
    );
    assert!(
        error
            .source_error()
            .downcast_ref::<crate::config::ConfigSaveError>()
            .is_some()
    );
    assert!(!error.to_string().contains("super-secret"));
    services.release_operation_event(Some(operation_id));
    services.shutdown();
}

#[test]
fn config_writer_keeps_the_newest_snapshot_when_workers_finish_out_of_order() {
    let root = unique_temp_dir("config-save-order");
    let config_dir = root.path().join("config");
    let mut old = AppConfig::default();
    old.general.confirm_quit = true;
    let mut new = old.clone();
    new.general.confirm_quit = false;
    let effects = vec![
        Effect::SaveConfig {
            request_id: 1,
            config: old,
            config_dir: config_dir.clone(),
        },
        Effect::SaveConfig {
            request_id: 2,
            config: new,
            config_dir: config_dir.clone(),
        },
    ];
    let services = AppServices::new().expect("services construction");
    let pending = pending_counter();
    execute_effects(effects, &services, &pending);

    for _ in 0..2 {
        let AppEvent::ConfigSaved { operation_id, .. } = services
            .events()
            .recv_timeout(EVENT_WAIT)
            .expect("config save completion")
        else {
            panic!("expected a config save completion");
        };
        services.release_operation_event(Some(operation_id));
    }

    assert!(!AppConfig::load(&config_dir).general.confirm_quit);
    services.shutdown();
}

#[test]
fn runtime_state_effect_dispatches_and_completes_on_the_worker() {
    let root = unique_temp_dir("runtime-state-effect");
    let data_dir = root.path().join("data");
    let mut app = App::new();
    app.set_config_paths(
        AppConfig::default(),
        root.path().join("config"),
        data_dir.clone(),
    );
    app.set_persistence_identity(true, Some(TrackLocation::local("/music/saved.mp3")), 12_000);

    let effects = app.persist_runtime_state();
    let [state_effect, _config_effect] = effects.as_slice() else {
        panic!("expected runtime state and config effects");
    };
    assert!(matches!(
        state_effect,
        Effect::SaveRuntimeState { request_id: 1, .. }
    ));

    let services = AppServices::new().expect("services construction");
    execute_effects(
        vec![state_effect.clone()],
        &services,
        &app.state().async_ops.pending_effects,
    );
    let AppEvent::RuntimeStateSaved {
        operation_id,
        request_id: 1,
        result: Ok(true),
    } = services
        .events()
        .recv_timeout(EVENT_WAIT)
        .expect("runtime state save completion")
    else {
        panic!("expected a successful runtime state save completion");
    };
    services.release_operation_event(Some(operation_id));

    let saved = PersistedState::load(&data_dir);
    assert_eq!(
        saved.last_track_path.as_deref(),
        Some("harmonium-local-v2:2f6d757369632f73617665642e6d7033")
    );
    assert_eq!(
        saved.track_location(),
        Some(TrackLocation::local("/music/saved.mp3"))
    );
    assert_eq!(saved.last_track_position_ms, 12_000);
    services.shutdown();
}

#[test]
fn runtime_state_writer_keeps_the_newest_snapshot_when_workers_finish_out_of_order() {
    let root = unique_temp_dir("runtime-state-order");
    let data_dir = root.path().join("data");
    let old = PersistedState {
        last_playlist: Some("Old".to_string()),
        ..PersistedState::default()
    };
    let new = PersistedState {
        last_playlist: Some("New".to_string()),
        ..PersistedState::default()
    };
    let writer = crate::runtime::RuntimeStateWriteCoordinator::default();

    assert!(writer.save(2, &new, &data_dir).expect("new state saves"));
    assert!(
        !writer
            .save(1, &old, &data_dir)
            .expect("stale state is skipped")
    );
    assert_eq!(
        PersistedState::load(&data_dir).last_playlist.as_deref(),
        Some("New")
    );
}

#[test]
fn failed_runtime_state_save_publishes_a_typed_error_without_a_notification() {
    let root = unique_temp_dir("runtime-state-failure");
    let data_dir = root.path().join("data");
    fs::write(&data_dir, "not a directory").expect("blocking state directory fixture");
    let services = AppServices::new().expect("services construction");

    execute_effects(
        vec![Effect::SaveRuntimeState {
            request_id: 1,
            state: PersistedState::default(),
            data_dir,
        }],
        &services,
        &pending_counter(),
    );
    let AppEvent::RuntimeStateSaved {
        operation_id,
        result: Err(error),
        ..
    } = services
        .events()
        .recv_timeout(EVENT_WAIT)
        .expect("runtime state save error completion")
    else {
        panic!("expected a typed runtime state save error");
    };
    assert!(error.to_string().contains("cannot create state directory"));
    services.release_operation_event(Some(operation_id));
    assert!(
        services.events().try_recv().is_err(),
        "state save failures must not become user notifications"
    );
    services.shutdown();
}

#[test]
fn completion_worker_panics_become_typed_worker_errors() {
    let services = AppServices::new().expect("services construction");
    for operation in [
        "browser-directory-validation",
        "runtime-state-save",
        "file-rename",
        "metadata-write",
    ] {
        let join_error = services.handle().block_on(async {
            tokio::task::spawn_blocking(|| panic!("completion worker panic"))
                .await
                .expect_err("blocking worker must panic")
        });
        let error = worker_panic_error(operation, join_error);
        assert_eq!(error.operation(), operation);
        assert!(
            error
                .source_error()
                .downcast_ref::<tokio::task::JoinError>()
                .is_some()
        );
        assert!(error.to_string().contains("completion worker panic"));
    }
    services.shutdown();
}

#[test]
fn runtime_state_effect_precedes_config_effect_without_changing_config_ordering() {
    let root = unique_temp_dir("runtime-state-config-order");
    let mut app = App::new();
    app.set_config_paths(
        AppConfig::default(),
        root.path().join("config"),
        root.path().join("data"),
    );

    let effects = app.persist_runtime_state();
    assert!(matches!(
        effects.as_slice(),
        [
            Effect::SaveRuntimeState { request_id: 1, .. },
            Effect::SaveConfig { request_id: 1, .. }
        ]
    ));
}

#[test]
fn browser_directory_effect_loads_filtered_entries_and_returns_a_result() {
    let root = unique_temp_dir("browser-effect");
    fs::write(root.join(".hidden.mp3"), "").expect("hidden fixture");
    fs::write(root.join("visible.mp3"), "").expect("visible fixture");

    let mut app = App::new();
    let effect = app.request_browser_dir(root.to_path_buf());
    let services = AppServices::new().expect("services construction");
    execute_effects(
        vec![effect],
        &services,
        &app.state().async_ops.pending_effects,
    );

    let event = services
        .events()
        .recv_timeout(EVENT_WAIT)
        .expect("browser result");
    let AppEvent::BrowserDirectoryLoaded {
        operation_id,
        request_id,
        dir,
        restore_cursor_name,
        entries,
    } = event
    else {
        panic!("expected a browser directory result");
    };
    assert!(services.accepts_operation_completion(operation_id));
    let save_effects =
        app.apply_browser_directory_loaded(request_id, dir, entries, restore_cursor_name);
    execute_effects(
        save_effects,
        &services,
        &app.state().async_ops.pending_effects,
    );
    assert_eq!(
        app.state()
            .browser
            .entries
            .iter()
            .map(|entry| entry.name.as_str())
            .collect::<Vec<_>>(),
        vec!["visible.mp3"]
    );
    services.release_operation_event(Some(operation_id));
    services.shutdown();
}

#[test]
fn browser_directory_validation_success_closes_editor_and_starts_listing() {
    let root = unique_temp_dir("browser-validation-success");
    let old_dir = root.join("old");
    let requested_dir = root.join("requested");
    fs::create_dir_all(&old_dir).expect("old directory");
    fs::create_dir_all(&requested_dir).expect("requested directory");

    let mut app = App::new();
    app.state_mut()
        .change_browser_dir(old_dir.clone(), Vec::new());
    app.handle_command(Command::OpenSettings);
    let effects = app.handle_key_event(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    complete_playlist_effects(&mut app, effects);
    app.state_mut()
        .popup_dialog
        .set_dialog_input(requested_dir.to_string_lossy().into_owned());

    let validation = app.handle_key_event(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    let [
        Effect::ValidateBrowserDirectory {
            request_id,
            path,
            validation,
        },
    ] = validation.as_slice()
    else {
        panic!("browser directory commit must validate on a worker");
    };
    let effects = app.apply_browser_directory_validated(
        *request_id,
        path.clone(),
        validation.clone(),
        Ok(path.clone()),
    );

    assert!(matches!(
        effects.as_slice(),
        [Effect::LoadBrowserDirectory { dir, .. }] if dir == &requested_dir
    ));
    assert_eq!(app.state().popup_dialog.dialog_mode_ref(), None);
    assert_eq!(
        app.state()
            .popup_dialog
            .active_popup_ref()
            .and_then(|popup| match popup {
                Popup::Settings { draft, .. } => Some(draft.browser_directory.clone()),
                _ => None,
            }),
        Some(requested_dir.to_string_lossy().into_owned())
    );
}

#[test]
fn browser_directory_validation_worker_accepts_a_directory_and_rejects_a_missing_path() {
    let root = unique_temp_dir("browser-validation-worker");
    let missing = root.join("missing");
    let mut app = App::new();
    app.handle_command(Command::OpenSettings);
    let effects = app.handle_key_event(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    complete_playlist_effects(&mut app, effects);
    app.state_mut()
        .popup_dialog
        .set_dialog_input(root.to_string_lossy().into_owned());
    let success = app.handle_key_event(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    let services = AppServices::new().expect("services construction");
    execute_effects(success, &services, &app.state().async_ops.pending_effects);
    let success_event = services
        .events()
        .recv_timeout(EVENT_WAIT)
        .expect("validation result");
    let AppEvent::BrowserDirectoryValidated {
        operation_id,
        result,
        ..
    } = success_event
    else {
        panic!("expected browser validation result");
    };
    assert!(services.accepts_operation_completion(operation_id));
    assert_eq!(result, Ok(root.to_path_buf()));
    services.release_operation_event(Some(operation_id));

    let mut app = App::new();
    app.handle_command(Command::OpenSettings);
    let effects = app.handle_key_event(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    complete_playlist_effects(&mut app, effects);
    app.state_mut()
        .popup_dialog
        .set_dialog_input(missing.to_string_lossy().into_owned());
    let failure = app.handle_key_event(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    execute_effects(failure, &services, &app.state().async_ops.pending_effects);
    let failure_event = services
        .events()
        .recv_timeout(EVENT_WAIT)
        .expect("validation error");
    let AppEvent::BrowserDirectoryValidated {
        operation_id,
        result,
        ..
    } = failure_event
    else {
        panic!("expected browser validation error");
    };
    assert!(services.accepts_operation_completion(operation_id));
    assert_eq!(
        result,
        Err(WorkerError::message(
            "browser-directory-validation",
            format!("Invalid path: {}", missing.to_string_lossy()),
        ))
    );
    services.release_operation_event(Some(operation_id));
    services.shutdown();
}

#[cfg(unix)]
#[test]
fn browser_symlink_resolution_stays_blocking_and_keeps_cursor_restore_context() {
    use std::os::unix::fs::symlink;

    let root = unique_temp_dir("browser-validation-symlink");
    let target = root.join("target");
    let link = root.join("link");
    fs::create_dir_all(&target).expect("target directory");
    symlink(&target, &link).expect("directory symlink");

    let mut app = App::new();
    app.state_mut().change_browser_dir(
        root.to_path_buf(),
        vec![crate::filesystem::FileEntry::new(
            "link",
            link.clone(),
            crate::filesystem::EntryKind::Symlink,
        )],
    );
    let effects = app.handle_command(Command::EnterSelected);
    let [
        Effect::ValidateBrowserDirectory {
            request_id,
            path,
            validation,
        },
    ] = effects.as_slice()
    else {
        panic!("symlink activation must validate on a worker");
    };
    assert!(matches!(
        validation,
        BrowserDirectoryValidation::Symlink { .. }
    ));
    let expected_request_id = *request_id;
    let expected_path = path.clone();

    let services = AppServices::new().expect("services construction");
    execute_effects(effects, &services, &app.state().async_ops.pending_effects);
    let AppEvent::BrowserDirectoryValidated {
        operation_id,
        request_id: completed_request_id,
        path: completed_path,
        validation,
        result,
    } = services
        .events()
        .recv_timeout(EVENT_WAIT)
        .expect("symlink validation result")
    else {
        panic!("expected symlink validation result");
    };
    assert_eq!(completed_request_id, expected_request_id);
    assert_eq!(completed_path, expected_path);
    assert!(services.accepts_operation_completion(operation_id));
    assert_eq!(result, Ok(target.clone()));
    let follow_up = app.apply_browser_directory_validated(
        completed_request_id,
        completed_path,
        validation,
        result,
    );
    assert!(matches!(
        follow_up.as_slice(),
        [Effect::LoadBrowserDirectory { dir, .. }] if dir == &target
    ));
    assert_eq!(
        app.state().browser.focus_stack,
        vec![(root.to_path_buf(), "link".to_string())]
    );
    services.release_operation_event(Some(operation_id));
    services.shutdown();
}

#[test]
fn stale_or_cancelled_browser_directory_validation_cannot_mutate_settings() {
    let root = unique_temp_dir("browser-validation-stale");
    let old_dir = root.join("old");
    let first_dir = root.join("first");
    let second_dir = root.join("second");
    fs::create_dir_all(&old_dir).expect("old directory");
    fs::create_dir_all(&first_dir).expect("first directory");
    fs::create_dir_all(&second_dir).expect("second directory");

    let mut app = App::new();
    app.state_mut()
        .change_browser_dir(old_dir.clone(), Vec::new());
    app.handle_command(Command::OpenSettings);
    let effects = app.handle_key_event(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    complete_playlist_effects(&mut app, effects);
    app.state_mut()
        .popup_dialog
        .set_dialog_input(first_dir.to_string_lossy().into_owned());
    let first = app.handle_key_event(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    app.state_mut()
        .popup_dialog
        .set_dialog_input(second_dir.to_string_lossy().into_owned());
    let second = app.handle_key_event(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

    let Effect::ValidateBrowserDirectory {
        request_id,
        path,
        validation,
    } = first.into_iter().next().expect("first validation")
    else {
        panic!("expected first validation effect");
    };
    assert!(matches!(
        second.as_slice(),
        [Effect::ValidateBrowserDirectory { .. }]
    ));
    assert!(
        app.apply_browser_directory_validated(
            request_id,
            path,
            validation,
            Err(WorkerError::message(
                "browser-directory-validation",
                "Invalid path: stale",
            )),
        )
        .is_empty()
    );
    assert!(app.state().popup_dialog.alert_message().is_none());
    assert_eq!(
        app.state().popup_dialog.dialog_mode_value(),
        Some(crate::state::DialogMode::SettingsEdit)
    );

    let Effect::ValidateBrowserDirectory { request_id, .. } =
        second.into_iter().next().expect("second validation")
    else {
        panic!("expected second validation effect");
    };
    app.handle_key_event(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    assert!(app.state().popup_dialog.dialog_mode_ref().is_none());
    assert!(
        app.apply_browser_directory_validated(
            request_id,
            PathBuf::from("/cancelled"),
            BrowserDirectoryValidation::Settings {
                input: "/cancelled".to_string(),
                previous_dir: old_dir,
            },
            Err(WorkerError::message(
                "browser-directory-validation",
                "Invalid path: /cancelled",
            )),
        )
        .is_empty()
    );
    assert!(app.state().popup_dialog.alert_message().is_none());
}

#[test]
fn playlist_effects_publish_typed_success_and_error_completions() {
    let dir = unique_temp_dir("playlist-effects");
    let store = crate::playlist::PlaylistStore::for_dir(dir.path());
    let mut app = App::from_config_and_store(crate::config::KeysConfig::default(), store.clone());
    app.state_mut()
        .popup_dialog
        .open_dialog(DialogMode::SaveAs, "Rock".to_string(), None);
    let mut services = AppServices::new().expect("services construction");
    services.set_playlist_store(Some(store));

    let listing_effects =
        app.request_named_save_validation(PlaylistSaveAction::SaveAs, "Rock".to_string());
    let [
        Effect::ListPlaylistNames {
            request_id,
            request,
        },
    ] = listing_effects.as_slice()
    else {
        panic!("expected playlist listing effect");
    };
    let listing_effect = Effect::ListPlaylistNames {
        request_id: *request_id,
        request: request.clone(),
    };
    execute_effects(
        vec![listing_effect],
        &services,
        &app.state().async_ops.pending_effects,
    );
    let AppEvent::PlaylistNamesCompleted {
        operation_id,
        request_id,
        request,
        result,
    } = services
        .events()
        .recv_timeout(EVENT_WAIT)
        .expect("playlist listing result")
    else {
        panic!("expected typed playlist listing result");
    };
    assert!(result.is_ok());
    let save_effects = app.apply_playlist_names_completed(request_id, request, result);
    if save_effects.len() != 1 {
        panic!("expected named save effect");
    }
    services.release_operation_event(Some(operation_id));

    execute_effects(
        save_effects,
        &services,
        &app.state().async_ops.pending_effects,
    );
    let AppEvent::PlaylistSaved {
        operation_id,
        request_id,
        name,
        action,
        result,
    } = services
        .events()
        .recv_timeout(EVENT_WAIT)
        .expect("playlist save result")
    else {
        panic!("expected typed playlist save result");
    };
    assert!(result.is_ok());
    app.apply_playlist_saved(request_id, name, action, result);
    services.release_operation_event(Some(operation_id));

    app.state_mut()
        .popup_dialog
        .open_dialog(DialogMode::SaveAs, "bad".to_string(), None);
    let request_id = app
        .state_mut()
        .async_ops
        .playlist_request
        .try_begin()
        .unwrap();
    let invalid_save = Effect::SavePlaylistNamed {
        request_id,
        name: "../bad".to_string(),
        contents: "#EXTM3U\n".to_string(),
        action: PlaylistSaveAction::SaveAs,
    };
    execute_effects(
        vec![invalid_save],
        &services,
        &app.state().async_ops.pending_effects,
    );
    let AppEvent::PlaylistSaved {
        operation_id,
        request_id,
        name,
        action,
        result,
    } = services
        .events()
        .recv_timeout(EVENT_WAIT)
        .expect("playlist save error result")
    else {
        panic!("expected typed playlist save error result");
    };
    assert!(result.is_err());
    app.apply_playlist_saved(request_id, name, action, result);
    assert!(
        app.state()
            .popup_dialog
            .dialog_error()
            .is_some_and(|error| error.contains("Save failed"))
    );
    services.release_operation_event(Some(operation_id));
    services.shutdown();
}

#[test]
fn playlist_load_effect_publishes_a_worker_owned_completion() {
    let dir = unique_temp_dir("playlist-load-worker");
    let store = crate::playlist::PlaylistStore::for_dir(dir.path());
    let mut target = crate::playlist::Playlist::new();
    target.extend([crate::track::Track::local("/music/target.mp3")]);
    store
        .save(&playlist_name("Target"), &target)
        .expect("seed playlist");

    let mut app = App::from_config_and_store(crate::config::KeysConfig::default(), store.clone());
    app.state_mut().active_playlist_name = Some("Current".to_string());
    app.state_mut()
        .popup_dialog
        .open_popup(Popup::PlaylistManager {
            cursor: 0,
            names: vec!["Target".to_string()],
        });
    let effects = app.handle_command(Command::LoadPlaylist);
    let [load_effect @ Effect::LoadPlaylistNamed { .. }] = effects.as_slice() else {
        panic!("expected named playlist load effect");
    };

    let mut services = AppServices::new().expect("services construction");
    services.set_playlist_store(Some(store));
    execute_effects(
        vec![load_effect.clone()],
        &services,
        &app.state().async_ops.pending_effects,
    );

    let AppEvent::PlaylistLoaded {
        operation_id,
        request_id,
        name,
        result,
    } = services
        .events()
        .recv_timeout(EVENT_WAIT)
        .expect("playlist load result")
    else {
        panic!("expected typed playlist load result");
    };
    assert!(result.is_ok());
    let completion_effects = app.apply_playlist_loaded(request_id, name, result);
    assert!(matches!(
        completion_effects.as_slice(),
        [Effect::Audio(AudioCommand::Stop), Effect::LoadMetadata(paths)]
            if paths == &vec![PathBuf::from("/music/target.mp3")]
    ));
    assert_eq!(app.state().active_playlist_name.as_deref(), Some("Target"));
    assert!(app.state().popup_dialog.active_popup_ref().is_none());
    services.release_operation_event(Some(operation_id));
    services.shutdown();
}

#[test]
fn failed_playlist_load_worker_preserves_queue_and_playback() {
    let dir = unique_temp_dir("playlist-load-worker-failure");
    let store = crate::playlist::PlaylistStore::for_dir(dir.path());
    let mut app = App::from_config_and_store(crate::config::KeysConfig::default(), store.clone());
    app.state_mut()
        .extend_playlist([PathBuf::from("/music/current.mp3")]);
    app.state_mut().active_playlist_name = Some("Current".to_string());
    app.state_mut().playback.status = PlayStatus::Playing;
    app.state_mut().playback.track_index = Some(0);
    app.state_mut().playback.elapsed = Duration::from_secs(9);
    let before_playlist = app.state().playlist.clone();
    let before_playback = app.state().playback.clone();
    app.state_mut()
        .popup_dialog
        .open_popup(Popup::PlaylistManager {
            cursor: 0,
            names: vec!["Missing".to_string()],
        });
    let load_effects = app.handle_command(Command::LoadPlaylist);
    let [load_effect @ Effect::LoadPlaylistNamed { .. }] = load_effects.as_slice() else {
        panic!("expected named playlist load effect");
    };

    let mut services = AppServices::new().expect("services construction");
    services.set_playlist_store(Some(store));
    execute_effects(
        vec![load_effect.clone()],
        &services,
        &app.state().async_ops.pending_effects,
    );
    let AppEvent::PlaylistLoaded {
        operation_id,
        request_id,
        name,
        result,
    } = services
        .events()
        .recv_timeout(EVENT_WAIT)
        .expect("playlist load error result")
    else {
        panic!("expected typed playlist load error result");
    };
    assert!(result.is_err());
    assert!(
        app.apply_playlist_loaded(request_id, name, result)
            .is_empty()
    );
    assert_eq!(app.state().playlist, before_playlist);
    assert_eq!(app.state().playback, before_playback);
    assert!(matches!(
        app.state().popup_dialog.active_popup_ref(),
        Some(Popup::PlaylistManager { .. })
    ));
    assert!(
        app.state()
            .notifications
            .last()
            .is_some_and(|message| message.contains("Could not load Missing"))
    );
    services.release_operation_event(Some(operation_id));
    services.shutdown();
}

#[test]
fn stale_or_cancelled_playlist_load_cannot_replace_a_newer_queue_or_popup() {
    let (_dir, mut app) = playlist_app("playlist-load-stale");
    app.state_mut()
        .extend_playlist([PathBuf::from("/music/current.mp3")]);
    app.state_mut().active_playlist_name = Some("Current".to_string());
    app.state_mut()
        .popup_dialog
        .open_popup(Popup::PlaylistManager {
            cursor: 0,
            names: vec!["Target".to_string()],
        });
    let load_effects = app.handle_command(Command::LoadPlaylist);
    let [Effect::LoadPlaylistNamed { request_id, name }] = load_effects.as_slice() else {
        panic!("expected named playlist load effect");
    };
    let stale_request_id = *request_id;
    let stale_name = name.clone();

    app.handle_command(Command::CancelPopup);
    app.state_mut()
        .extend_playlist([PathBuf::from("/music/newer.mp3")]);
    app.state_mut()
        .popup_dialog
        .open_popup(Popup::PlaylistManager {
            cursor: 0,
            names: vec!["Newer".to_string()],
        });
    let newer_effects = app.handle_command(Command::LoadPlaylist);
    assert!(matches!(
        newer_effects.as_slice(),
        [Effect::LoadPlaylistNamed { request_id, .. }] if *request_id != stale_request_id
    ));

    let mut stale_playlist = crate::playlist::Playlist::new();
    stale_playlist.extend([crate::track::Track::local("/music/stale.mp3")]);
    assert!(
        app.apply_playlist_loaded(stale_request_id, stale_name, Ok(stale_playlist))
            .is_empty()
    );
    assert_eq!(
        app.state().playlist.tracks()[1].display_location(),
        "/music/newer.mp3"
    );
    assert!(matches!(
        app.state().popup_dialog.active_popup_ref(),
        Some(Popup::PlaylistManager { .. })
    ));
}

#[test]
fn superseded_playlist_load_operation_is_rejected_by_the_runtime_guard() {
    let dir = unique_temp_dir("playlist-load-operation-stale");
    let store = crate::playlist::PlaylistStore::for_dir(dir.path());
    store
        .save(&playlist_name("First"), &crate::playlist::Playlist::new())
        .expect("seed first playlist");
    store
        .save(&playlist_name("Second"), &crate::playlist::Playlist::new())
        .expect("seed second playlist");
    let mut app = App::from_config_and_store(crate::config::KeysConfig::default(), store.clone());
    let mut services = AppServices::new().expect("services construction");
    services.set_playlist_store(Some(store));

    app.state_mut()
        .popup_dialog
        .open_popup(Popup::PlaylistManager {
            cursor: 0,
            names: vec!["First".to_string()],
        });
    let first_effects = app.handle_command(Command::LoadPlaylist);
    execute_effects(
        first_effects,
        &services,
        &app.state().async_ops.pending_effects,
    );
    let AppEvent::PlaylistLoaded {
        operation_id: first_operation_id,
        ..
    } = services
        .events()
        .recv_timeout(EVENT_WAIT)
        .expect("first playlist load result")
    else {
        panic!("expected first playlist load result");
    };

    app.handle_command(Command::CancelPopup);
    app.state_mut()
        .popup_dialog
        .open_popup(Popup::PlaylistManager {
            cursor: 0,
            names: vec!["Second".to_string()],
        });
    let second_effects = app.handle_command(Command::LoadPlaylist);
    execute_effects(
        second_effects,
        &services,
        &app.state().async_ops.pending_effects,
    );
    assert!(!services.accepts_operation_completion(first_operation_id));
    services.release_operation_event(Some(first_operation_id));

    let AppEvent::PlaylistLoaded {
        operation_id: second_operation_id,
        ..
    } = services
        .events()
        .recv_timeout(EVENT_WAIT)
        .expect("second playlist load result")
    else {
        panic!("expected second playlist load result");
    };
    assert_ne!(first_operation_id, second_operation_id);
    services.release_operation_event(Some(second_operation_id));
    services.shutdown();
}

#[test]
fn playlist_rename_and_delete_effects_publish_worker_owned_results() {
    let dir = unique_temp_dir("playlist-mutations");
    let store = crate::playlist::PlaylistStore::for_dir(dir.path());
    store
        .save(&playlist_name("Rock"), &crate::playlist::Playlist::new())
        .expect("seed playlist");
    let mut app = App::from_config_and_store(crate::config::KeysConfig::default(), store.clone());
    app.state_mut().active_playlist_name = Some("Rock".to_string());
    let mut services = AppServices::new().expect("services construction");
    services.set_playlist_store(Some(store));

    let rename_request = app
        .state_mut()
        .async_ops
        .playlist_request
        .try_begin()
        .unwrap();
    execute_effects(
        vec![Effect::RenamePlaylistNamed {
            request_id: rename_request,
            old_name: "Rock".to_string(),
            new_name: "Jazz".to_string(),
            action: PlaylistRenameAction::Playing,
        }],
        &services,
        &app.state().async_ops.pending_effects,
    );
    let AppEvent::PlaylistRenamed {
        operation_id,
        request_id,
        old_name,
        new_name,
        action,
        result,
    } = services
        .events()
        .recv_timeout(EVENT_WAIT)
        .expect("playlist rename result")
    else {
        panic!("expected typed playlist rename result");
    };
    assert_eq!(result, PlaylistRenameResult::Success);
    app.apply_playlist_renamed(request_id, old_name, new_name, action, result);
    services.release_operation_event(Some(operation_id));

    let delete_request = app
        .state_mut()
        .async_ops
        .playlist_request
        .try_begin()
        .unwrap();
    execute_effects(
        vec![Effect::DeletePlaylistNamed {
            request_id: delete_request,
            name: "Jazz".to_string(),
            cursor: None,
            was_active: true,
        }],
        &services,
        &app.state().async_ops.pending_effects,
    );
    let AppEvent::PlaylistDeleted {
        operation_id,
        request_id,
        name,
        cursor,
        was_active,
        result,
    } = services
        .events()
        .recv_timeout(EVENT_WAIT)
        .expect("playlist delete result")
    else {
        panic!("expected typed playlist delete result");
    };
    assert_eq!(result.deletion, Ok(()));
    assert!(
        !result
            .names
            .as_ref()
            .expect("playlist refresh")
            .iter()
            .any(|candidate| candidate == "Jazz")
    );
    app.apply_playlist_deleted(request_id, name, cursor, was_active, result);
    services.release_operation_event(Some(operation_id));
    assert!(app.playlist_store().load(&playlist_name("Jazz")).is_err());
    services.shutdown();
}

#[test]
fn source_ready_drops_a_stale_event_for_a_replaced_track() {
    let mut app = App::new();
    let url = url::Url::parse("https://example.com/live").expect("valid url");
    let track = crate::track::Track::from_stream(url.clone(), crate::stream::StreamKind::Http);
    {
        let state = app.state_mut();
        state.playlist = {
            let mut pl = crate::playlist::Playlist::new();
            pl.extend([track]);
            pl
        };
        state.playlist.select(0);
    }
    let _ = app.begin_current_track();

    // The audio engine reports a SourceReady for a track the user
    // already replaced: a stale delivery must not clear the flag
    // because the new track may still be loading.
    app.apply_source_ready("https://other.example.com/live".to_string());
    assert!(
        app.state().async_ops.stream_activity().is_some(),
        "stale SourceReady events must leave the flag set"
    );
}

#[test]
fn begin_current_track_sets_artwork_loading_for_local_tracks() {
    let mut app = App::new();
    app.state_mut().artwork.set_enabled(true);
    app.state_mut().artwork.loading = false;
    let path = PathBuf::from("/music/song.mp3");
    app.state.playlist = {
        let mut pl = crate::playlist::Playlist::new();
        pl.extend([crate::track::Track::local(path)]);
        pl
    };
    app.state.playlist.select(0);

    let effects = app.begin_current_track();
    assert!(
        effects
            .iter()
            .any(|e| matches!(e, Effect::LoadArtwork { .. })),
        "begin_current_track must dispatch LoadArtwork when artwork is enabled"
    );
    assert!(
        app.state().artwork.loading,
        "the dispatch must raise the artwork_loading flag"
    );
}

/// Regression test for the spinner not appearing during stream decode.
///
/// `begin_current_track` is the central choke point for starting any
/// track (manual play, auto-advance, next, previous). When the track
/// is a stream the audio engine takes 1-50 s to build the decoder
/// and the UI must raise stream activity so the Now Playing band
/// swaps its progress bar for the spinner. Before this test the flag
/// only fired from the Add Stream popup, so any stream already in
/// the queue showed no feedback during decode.
#[test]
fn begin_current_track_raises_stream_activity_for_streams() {
    let mut app = App::new();
    let url = url::Url::parse("https://www.youtube.com/watch?v=abc").expect("valid url");
    app.state.playlist = {
        let mut pl = crate::playlist::Playlist::new();
        pl.extend([crate::track::Track::from_stream(
            url,
            crate::stream::StreamKind::Http,
        )]);
        pl
    };
    app.state.playlist.select(0);

    let effects = app.begin_current_track();
    assert!(
        effects
            .iter()
            .any(|e| matches!(e, Effect::Audio(AudioCommand::Play { .. }))),
        "begin_current_track must dispatch AudioCommand::Play"
    );
    assert!(
        app.state().async_ops.stream_activity().is_some(),
        "begin_current_track must raise stream activity while the audio engine decodes the stream"
    );
}

/// A local track supersedes a pending stream acquisition and clears its UI
/// identity before the local Play command is dispatched.
#[test]
fn begin_current_track_replaces_stream_with_local_and_clears_stream_activity() {
    let mut app = App::new();
    let stream_url = url::Url::parse("https://example.com/stream").expect("valid url");
    let path = PathBuf::from("/music/song.mp3");
    app.state.playlist = {
        let mut pl = crate::playlist::Playlist::new();
        pl.extend([
            crate::track::Track::from_stream(stream_url, crate::stream::StreamKind::Http),
            crate::track::Track::local(path.clone()),
        ]);
        pl
    };
    app.state.playlist.select(0);
    let _ = app.begin_current_track();
    assert!(app.state().async_ops.stream_activity().is_some());
    assert_eq!(
        match app.state().async_ops.stream_activity() {
            Some(StreamActivity::Acquiring { source, .. }) => Some(source),
            _ => None,
        },
        Some(&TrackLocation::url(
            url::Url::parse("https://example.com/stream").expect("valid URL")
        ))
    );

    app.state.playlist.select(1);
    let effects = app.begin_current_track();

    assert!(
        app.state().async_ops.stream_activity().is_none(),
        "a local Play must clear the superseded stream spinner"
    );
    assert!(
        app.state().async_ops.stream_activity().is_none(),
        "a local Play must clear the superseded stream identity"
    );
    assert!(matches!(
        effects.first(),
        Some(Effect::Audio(AudioCommand::Play {
            source: crate::stream::TrackSource::Local(actual),
            track_index: 1,
            ..
        })) if TrackLocation::Local(actual.clone()) == TrackLocation::local(path.clone())
    ));
}

#[test]
fn begin_current_track_repressing_same_stream_restarts_stream_activity_identity() {
    let mut app = App::new();
    let url = url::Url::parse("https://example.com/stream").expect("valid url");
    app.state.playlist = {
        let mut pl = crate::playlist::Playlist::new();
        pl.extend([crate::track::Track::from_stream(
            url,
            crate::stream::StreamKind::Http,
        )]);
        pl
    };
    app.state.playlist.select(0);

    let _ = app.begin_current_track();
    let first_identity = match app.state().async_ops.stream_activity() {
        Some(StreamActivity::Acquiring { source, .. }) => Some(source.clone()),
        _ => None,
    };
    let first_generation = app
        .state()
        .async_ops
        .stream_activity()
        .and_then(|activity| match activity {
            StreamActivity::Acquiring { generation, .. } => Some(*generation),
            StreamActivity::Resolving { .. } => None,
        })
        .expect("first stream start has a generation");
    let _ = app.begin_current_track();
    let second_generation = app
        .state()
        .async_ops
        .stream_activity()
        .and_then(|activity| match activity {
            StreamActivity::Acquiring { generation, .. } => Some(*generation),
            StreamActivity::Resolving { .. } => None,
        })
        .expect("restart has a generation");

    assert!(app.state().async_ops.stream_activity().is_some());
    assert_ne!(first_generation, second_generation);
    assert_eq!(
        match app.state().async_ops.stream_activity() {
            Some(StreamActivity::Acquiring { source, .. }) => Some(source.clone()),
            _ => None,
        },
        first_identity,
        "restarting the same stream keeps the active URL identity"
    );
}

#[test]
fn stale_same_url_source_ready_does_not_clear_a_newer_restart() {
    let mut app = App::new();
    let url = url::Url::parse("https://example.com/stream").expect("valid url");
    app.state.playlist = {
        let mut pl = crate::playlist::Playlist::new();
        pl.extend([crate::track::Track::from_stream(
            url.clone(),
            crate::stream::StreamKind::Http,
        )]);
        pl
    };
    app.state.playlist.select(0);

    let _ = app.begin_current_track();
    let first_generation = app
        .state()
        .async_ops
        .stream_activity()
        .and_then(|activity| match activity {
            StreamActivity::Acquiring { generation, .. } => Some(*generation),
            StreamActivity::Resolving { .. } => None,
        })
        .expect("first stream start has a generation");
    let _ = app.begin_current_track();
    let second_generation = app
        .state()
        .async_ops
        .stream_activity()
        .and_then(|activity| match activity {
            StreamActivity::Acquiring { generation, .. } => Some(*generation),
            StreamActivity::Resolving { .. } => None,
        })
        .expect("restart has a generation");

    app.apply_source_ready_with_generation(first_generation, url.to_string());
    assert!(app.state().async_ops.stream_activity().is_some());
    assert_eq!(
        match app.state().async_ops.stream_activity() {
            Some(StreamActivity::Acquiring { generation, .. }) => Some(*generation),
            _ => None,
        },
        Some(second_generation)
    );

    app.apply_source_ready_with_generation(second_generation, url.to_string());
    assert!(app.state().async_ops.stream_activity().is_none());
}

#[test]
fn stale_same_url_source_failed_does_not_clear_a_newer_restart() {
    let mut app = App::new();
    let url = url::Url::parse("https://example.com/stream").expect("valid url");
    app.state.playlist = {
        let mut pl = crate::playlist::Playlist::new();
        pl.extend([crate::track::Track::from_stream(
            url.clone(),
            crate::stream::StreamKind::Http,
        )]);
        pl
    };
    app.state.playlist.select(0);

    let _ = app.begin_current_track();
    let first_generation = app
        .state()
        .async_ops
        .stream_activity()
        .and_then(|activity| match activity {
            StreamActivity::Acquiring { generation, .. } => Some(*generation),
            StreamActivity::Resolving { .. } => None,
        })
        .expect("first stream start has a generation");
    let _ = app.begin_current_track();
    let second_generation = app
        .state()
        .async_ops
        .stream_activity()
        .and_then(|activity| match activity {
            StreamActivity::Acquiring { generation, .. } => Some(*generation),
            StreamActivity::Resolving { .. } => None,
        })
        .expect("restart has a generation");

    app.apply_source_failed_with_generation(first_generation, url.to_string());
    assert!(app.state().async_ops.stream_activity().is_some());
    assert_eq!(
        match app.state().async_ops.stream_activity() {
            Some(StreamActivity::Acquiring { generation, .. }) => Some(*generation),
            _ => None,
        },
        Some(second_generation)
    );

    app.apply_source_failed_with_generation(second_generation, url.to_string());
    assert!(app.state().async_ops.stream_activity().is_none());
}

#[test]
fn stale_artwork_delivery_does_not_clear_a_newer_pending_request() {
    // A late artwork delivery for a track the user already left must not
    // paint over the new track or stop its still-active loading indicator.
    let mut app = App::new();
    app.state_mut().artwork.loading = true;
    app.state.playback.track_index = Some(2);

    app.apply_artwork_loaded(0, None);
    assert!(
        app.state().artwork.loading,
        "a stale artwork delivery must not clear the current request"
    );
}

fn q_key() -> KeyEvent {
    KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE)
}

fn y_key() -> KeyEvent {
    KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE)
}

fn esc_key() -> KeyEvent {
    KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)
}

#[test]
fn active_popup_ref_borrows_the_popup_stored_in_state() {
    let mut app = App::new();
    app.handle_command(Command::Quit);

    let borrowed = app.active_popup_ref().expect("quit popup should be open");
    let stored = app
        .state()
        .popup_dialog
        .active_popup_ref()
        .expect("quit popup should remain stored");

    assert!(std::ptr::eq(borrowed, stored));
}

#[test]
fn active_popup_owned_preserves_an_owned_snapshot() {
    let mut app = App::new();
    app.handle_command(Command::Quit);

    let owned = app
        .active_popup_owned()
        .expect("quit popup should be available to owned callers");
    app.state_mut().popup_dialog.clear();

    assert_eq!(owned, Popup::ConfirmQuit);
}

#[test]
fn quit_with_default_confirmation_opens_dialog_only() {
    let mut app = App::new();
    assert!(app.state().confirm_quit);

    app.handle_command(Command::Quit);

    assert_eq!(app.active_popup(), Some(Popup::ConfirmQuit));
    assert!(!app.should_quit());
}

#[test]
fn quit_without_confirmation_exits_directly() {
    let mut app = App::new();
    app.state.confirm_quit = false;

    app.handle_command(Command::Quit);

    assert!(app.should_quit());
    assert!(app.active_popup().is_none());
}

#[test]
fn confirming_quit_closes_dialog_and_exits() {
    let mut app = App::new();
    app.handle_command(Command::Quit);

    app.handle_command(Command::ConfirmQuitYes);

    assert!(app.should_quit());
    assert!(app.active_popup().is_none());
}

#[test]
fn cancelling_quit_clears_dialog_without_exiting() {
    let mut app = App::new();
    app.handle_command(Command::Quit);

    app.handle_command(Command::CancelPopup);

    assert!(!app.should_quit());
    assert!(app.active_popup().is_none());
}

#[test]
fn escape_key_cancels_pending_confirmation() {
    let mut app = App::new();
    app.handle_command(Command::Quit);

    app.handle_key_event(esc_key());

    assert!(!app.should_quit());
    assert!(app.active_popup().is_none());
}

#[test]
fn full_quit_flow_through_key_events() {
    let mut app = App::new();

    app.handle_key_event(q_key());
    assert_eq!(app.active_popup(), Some(Popup::ConfirmQuit));

    app.handle_key_event(y_key());
    assert!(app.should_quit());
}

#[test]
fn focus_cycles_forward_across_panels() {
    let mut app = App::new();
    assert_eq!(app.active_panel(), Panel::Browser);

    app.handle_key_event(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
    assert_eq!(app.active_panel(), Panel::Playlist);

    app.handle_key_event(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
    assert_eq!(app.active_panel(), Panel::Browser);
}

#[test]
fn focus_cycles_backward_across_panels() {
    let mut app = App::new();

    app.handle_command(Command::FocusPreviousPanel);
    assert_eq!(app.active_panel(), Panel::Playlist);

    app.handle_command(Command::FocusPreviousPanel);
    assert_eq!(app.active_panel(), Panel::Browser);
}

#[test]
fn notification_queue_honors_the_display_cap() {
    let mut app = App::new();

    for index in 0..55 {
        app.push_notification(format!("note-{index}"));
    }

    assert_eq!(app.state().notifications.len(), NOTIFICATIONS_CAP);
    assert_eq!(
        app.state().notifications.first(),
        Some(&"note-5".to_string())
    );
}

#[test]
fn input_context_reports_the_focused_panel() {
    let mut app = App::new();

    assert!(app.input_context().browser_focused);

    app.handle_key_event(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
    assert!(!app.input_context().browser_focused);
}

#[test]
fn scan_completion_appends_tracks_and_requests_metadata() {
    let mut app = App::new();
    let dir = PathBuf::from("/music/album");

    let effects = app.apply_scan_completed(
        dir.clone(),
        vec![
            PathBuf::from("/music/a.mp3"),
            PathBuf::from("/music/b.flac"),
        ],
    );

    assert_eq!(app.state().playlist.len(), 2);
    assert_eq!(
        effects,
        vec![Effect::LoadMetadata(vec![
            PathBuf::from("/music/a.mp3"),
            PathBuf::from("/music/b.flac"),
        ])]
    );
    assert_eq!(
        app.state().notifications.last(),
        Some(&format!("Added 2 tracks from {}", dir.display()))
    );
}

#[test]
fn scan_completion_requests_metadata_for_entries_added_after_a_middle_duplicate() {
    let mut app = App::new();
    app.state.extend_playlist([PathBuf::from("/music/a.mp3")]);

    let effects = app.apply_scan_completed(
        PathBuf::from("/music/album"),
        vec![
            PathBuf::from("/music/a.mp3"),
            PathBuf::from("/music/b.mp3"),
            PathBuf::from("/music/c.mp3"),
        ],
    );

    let queued_paths = app
        .state()
        .playlist
        .tracks()
        .iter()
        .filter_map(|track| track.path().map(Path::to_path_buf))
        .collect::<Vec<_>>();
    assert_eq!(
        queued_paths,
        vec![
            PathBuf::from("/music/a.mp3"),
            PathBuf::from("/music/b.mp3"),
            PathBuf::from("/music/c.mp3"),
        ]
    );
    assert_eq!(
        effects,
        vec![Effect::LoadMetadata(vec![
            PathBuf::from("/music/b.mp3"),
            PathBuf::from("/music/c.mp3"),
        ])]
    );
}

#[test]
fn empty_scan_completion_reports_no_supported_files() {
    let mut app = App::new();

    let effects = app.apply_scan_completed(PathBuf::from("/empty"), Vec::new());

    assert!(app.state().playlist.is_empty());
    assert!(effects.is_empty());
    assert_eq!(
        app.state().notifications.last(),
        Some(&"No supported audio files found".to_string())
    );
}

#[test]
fn metadata_results_attach_to_every_matching_queue_entry() {
    let mut app = App::new();
    // Duplicates are skipped, so `/dup.mp3` lands exactly once.
    let append = app.state.extend_playlist([
        PathBuf::from("/dup.mp3"),
        PathBuf::from("/other.mp3"),
        PathBuf::from("/dup.mp3"),
    ]);
    assert_eq!(append.added(), 2);
    assert_eq!(append.skipped, 1, "the repeat must be skipped");

    let meta = TrackMetadata {
        title: "Snapshot".to_string(),
        title_tagged: true,
        artist: "A".to_string(),
        album: "L".to_string(),
        track_number: None,
        duration: std::time::Duration::from_secs(9),
        bitrate: None,
        sample_rate: None,
        codec: String::new(),
        format: String::new(),
        album_artist: None,
        disc_number: None,
        genre: None,
        year: None,
        composer: None,
        comment: None,
    };
    app.apply_metadata_completed(vec![(PathBuf::from("/dup.mp3"), meta)], 1);

    assert_eq!(app.state().playlist.len(), 2);
    assert_eq!(app.state().playlist.tracks()[0].display_name(), "Snapshot");
    // Untouched entries keep their stem fallback
    assert_eq!(app.state().playlist.tracks()[1].display_name(), "other");
    assert_eq!(
        app.state().notifications.last(),
        Some(&"Metadata unavailable for 1 track".to_string())
    );
}

#[test]
fn metadata_failures_pluralize_the_status_message() {
    let mut app = App::new();

    app.apply_metadata_completed(Vec::new(), 3);
    assert_eq!(
        app.state().notifications.last(),
        Some(&"Metadata unavailable for 3 tracks".to_string())
    );

    app.apply_metadata_completed(Vec::new(), 0);
    assert_eq!(
        app.state().notifications.last(),
        Some(&"Metadata unavailable for 3 tracks".to_string()),
        "a clean batch must not notify"
    );
}

#[test]
fn late_metadata_syncs_duration_for_current_track() {
    use crate::playlist::Playlist;
    use crate::track::Track;

    let mut app = App::new();
    let path = PathBuf::from("/playing.mp3");
    let mut playlist = Playlist::new();
    playlist.extend([Track::local(path.clone())]);
    app.state.playlist = playlist;
    // Track is playing but metadata has not arrived yet
    app.state.playback.track_index = Some(0);
    app.state.playback.status = PlayStatus::Playing;
    assert!(app.state.playback.duration.is_none());

    let meta = TrackMetadata {
        title: "Late Track".to_string(),
        title_tagged: true,
        artist: "A".to_string(),
        album: "B".to_string(),
        track_number: None,
        duration: std::time::Duration::from_secs(210),
        bitrate: None,
        sample_rate: None,
        codec: String::new(),
        format: String::new(),
        album_artist: None,
        disc_number: None,
        genre: None,
        year: None,
        composer: None,
        comment: None,
    };
    app.apply_metadata_completed(vec![(path, meta)], 0);

    assert_eq!(
        app.state().playback.duration,
        Some(std::time::Duration::from_secs(210)),
        "late metadata must update the gauge duration for the current track"
    );
}

#[test]
fn late_metadata_does_not_overwrite_existing_duration() {
    use crate::playlist::Playlist;
    use crate::track::Track;

    let mut app = App::new();
    let path = PathBuf::from("/playing.mp3");
    let mut playlist = Playlist::new();
    playlist.extend([Track::local(path.clone())]);
    app.state.playlist = playlist;
    app.state.playback.track_index = Some(0);
    app.state.playback.duration = Some(std::time::Duration::from_secs(180));

    let meta = TrackMetadata {
        title: "Late".to_string(),
        title_tagged: true,
        artist: "A".to_string(),
        album: "B".to_string(),
        track_number: None,
        duration: std::time::Duration::from_secs(210),
        bitrate: None,
        sample_rate: None,
        codec: String::new(),
        format: String::new(),
        album_artist: None,
        disc_number: None,
        genre: None,
        year: None,
        composer: None,
        comment: None,
    };
    app.apply_metadata_completed(vec![(path, meta)], 0);

    assert_eq!(
        app.state().playback.duration,
        Some(std::time::Duration::from_secs(180)),
        "must not overwrite duration that was already set"
    );
}

#[test]
fn metadata_apply_preserves_the_stored_playlist_order() {
    use crate::metadata::TrackMetadata;
    use crate::playlist::Playlist;
    use crate::track::Track;

    let mut app = App::new();
    app.config.general.playlist_columns = crate::config::PlaylistColumnsConfig {
        display_by: crate::config::SortBy::Metadata,
        metadata_track_number: true,
        ..Default::default()
    };
    let path_first = PathBuf::from("/music/track1.mp3");
    let path_second = PathBuf::from("/music/track2.mp3");
    app.state.playlist = {
        let mut pl = Playlist::new();
        pl.extend([
            Track::local(path_second.clone()),
            Track::local(path_first.clone()),
        ]);
        pl
    };

    let metadata_with_track_number = |number| TrackMetadata {
        title: format!("Track {number}"),
        title_tagged: true,
        artist: "A".to_string(),
        album: "B".to_string(),
        track_number: Some(number),
        duration: std::time::Duration::from_secs(60),
        bitrate: None,
        sample_rate: None,
        codec: String::new(),
        format: String::new(),
        album_artist: None,
        disc_number: None,
        genre: None,
        year: None,
        composer: None,
        comment: None,
    };
    app.apply_metadata_completed(
        vec![
            (path_second.clone(), metadata_with_track_number(2)),
            (path_first.clone(), metadata_with_track_number(1)),
        ],
        0,
    );

    let ordered: Vec<String> = app
        .state()
        .playlist
        .tracks()
        .iter()
        .map(|t| t.display_location().into_owned())
        .collect();
    assert_eq!(ordered, vec!["/music/track2.mp3", "/music/track1.mp3"]);
}

#[test]
fn metadata_completion_updates_labels_without_reordering_or_autosaving() {
    use crate::config::{PlaylistColumnsConfig, SortBy};
    use crate::playlist::Playlist;
    use crate::track::Track;

    let mut app = App::new();
    app.state.playlist_columns = PlaylistColumnsConfig {
        display_by: SortBy::Metadata,
        metadata_artist: true,
        metadata_album: true,
        metadata_track_number: true,
    };
    app.state.active_playlist_name = Some("Restored".to_string());

    let paths = [
        "/music/artist-a.mp3",
        "/music/album-a.mp3",
        "/music/track-one.mp3",
        "/music/track-two.mp3",
        "/music/artist-z.mp3",
    ]
    .map(PathBuf::from);
    let mut playlist = Playlist::new();
    for (index, path) in paths.iter().enumerate() {
        let mut track = Track::local(path.clone());
        track.set_metadata(TrackMetadata {
            title: format!("Title {index}"),
            title_tagged: true,
            ..TrackMetadata::default()
        });
        playlist.extend([track]);
    }
    app.state.playlist = playlist;

    let request_effects = app.load_metadata_for_queue();
    assert_eq!(
        request_effects,
        vec![Effect::LoadMetadata(paths.to_vec())],
        "provisional EXTINF metadata must not suppress real metadata loading"
    );

    let metadata = |artist: &str, album: &str, number: u32, title: &str| TrackMetadata {
        title: title.to_string(),
        title_tagged: true,
        artist: artist.to_string(),
        album: album.to_string(),
        track_number: Some(number),
        duration: Duration::from_secs(60),
        ..TrackMetadata::default()
    };
    let save_effects = app.apply_metadata_completed(
        vec![
            (paths[0].clone(), metadata("Z", "A", 1, "A title")),
            (paths[1].clone(), metadata("Same", "A", 99, "Z title")),
            (paths[2].clone(), metadata("Same", "Z", 1, "Z title")),
            (paths[3].clone(), metadata("Same", "Z", 2, "A title")),
            (paths[4].clone(), metadata("A", "Z", 99, "Z title")),
        ],
        0,
    );

    let ordered: Vec<String> = app
        .state
        .playlist
        .tracks()
        .iter()
        .map(|track| track.display_location().into_owned())
        .collect();
    assert_eq!(
        ordered,
        vec![
            "/music/artist-a.mp3",
            "/music/album-a.mp3",
            "/music/track-one.mp3",
            "/music/track-two.mp3",
            "/music/artist-z.mp3",
        ],
        "metadata completion must preserve the stored playlist order"
    );
    assert!(
        save_effects.is_empty(),
        "metadata completion is not a sort action"
    );
    let labels: Vec<String> = app
        .state
        .playlist
        .tracks()
        .iter()
        .map(|track| crate::playlist::sorter::display_label(track, &app.state.playlist_columns))
        .collect();
    assert_eq!(
        labels,
        vec![
            "Z - A - 1 - A title",
            "Same - A - 99 - Z title",
            "Same - Z - 1 - Z title",
            "Same - Z - 2 - A title",
            "A - Z - 99 - Z title",
        ],
        "metadata completion must refresh playlist labels in place"
    );
}

/// Regression test for the separated playlist-columns model: saving
/// Settings persists the display configuration without reordering the
/// stored queue.
#[test]
fn apply_settings_persists_playlist_columns_without_reordering() {
    use crate::config::{PlaylistColumnsConfig, SortBy};
    use crate::playlist::Playlist;
    use crate::track::Track;

    let mut app = App::new();
    // Config already at Metadata + Album+Title.
    let columns = PlaylistColumnsConfig {
        display_by: SortBy::Metadata,
        metadata_album: true,
        ..PlaylistColumnsConfig::default()
    };
    app.config.general.playlist_columns = columns.clone();
    app.state.playlist_columns = columns.clone();

    let meta = |title: &str, album: &str| TrackMetadata {
        title: title.to_string(),
        title_tagged: true,
        artist: String::new(),
        album: album.to_string(),
        track_number: None,
        duration: std::time::Duration::from_secs(60),
        bitrate: None,
        sample_rate: None,
        codec: String::new(),
        format: String::new(),
        album_artist: None,
        disc_number: None,
        genre: None,
        year: None,
        composer: None,
        comment: None,
    };

    // Build three tracks with metadata attached, then insert them into
    // the playlist in a deliberately stale order. Bypassing
    // `apply_metadata_completed` keeps that order intact so Settings close
    // can be tested in isolation.
    let mut track_yesterday = Track::local("/music/help-yesterday.mp3");
    track_yesterday.set_metadata(meta("Yesterday", "Help"));
    let mut track_time = Track::local("/music/dark-side-time.mp3");
    track_time.set_metadata(meta("Time", "Dark Side"));
    let mut track_overture = Track::local("/music/2112-overture.mp3");
    track_overture.set_metadata(meta("Overture", "2112"));

    app.state.playlist = {
        let mut pl = Playlist::new();
        pl.extend([track_yesterday, track_time, track_overture]);
        pl
    };

    // Pre-condition: the playlist is intentionally not in the configured
    // presentation order.
    let before: Vec<String> = app
        .state()
        .playlist
        .tracks()
        .iter()
        .map(|t| t.display_location().into_owned())
        .collect();
    assert_eq!(
        before,
        vec![
            "/music/help-yesterday.mp3".to_string(),
            "/music/dark-side-time.mp3".to_string(),
            "/music/2112-overture.mp3".to_string(),
        ],
        "precondition: playlist is stale-ordered by title"
    );

    // Build the Settings draft by hand so the test does not depend on
    // the `OpenSettings` command's plumbing; we only care about
    // `apply_settings` here, which is the choke point the user reaches
    // by pressing Esc inside the modal.
    let themes_dir = std::path::PathBuf::new();
    let mut draft = crate::state::SettingsDraft::from_state(&app.state, &app.config, &themes_dir);
    draft.playlist_columns = columns.clone();
    draft.playlist_columns_initial = columns.clone();
    app.state.popup_dialog.open_popup(Popup::Settings {
        tab: crate::state::SettingsTab::General,
        focus: crate::state::SettingsFocus::Content,
        draft,
    });

    let _ = app.apply_settings();
    assert!(
        app.active_popup().is_none(),
        "apply_settings must close the popup"
    );

    // Closing Settings persists the presentation config but never sorts.
    let after: Vec<String> = app
        .state()
        .playlist
        .tracks()
        .iter()
        .map(|t| t.display_location().into_owned())
        .collect();
    assert_eq!(after, before, "Settings save must not reorder the playlist");
    assert_eq!(app.state().playlist_columns, columns);
}

/// Regression test for the playing-track indicator on mixed playlists.
/// When a stream is playing and the user reorders (by Filename or Metadata),
/// the row indicator (`▶`) must follow the stream to its new position
/// instead of staying on the stale pre-sort index, where a different
/// track now sits.
///
/// `apply_sort_tracks` captures the playing track's `track_location()` (the URL
/// for streams, the path for local files) before the sort, then re-anchors
/// `state.playback.track_index` to whatever row carries that same location
/// afterwards. If the reanchor fails — or never runs — `track_index`
/// keeps pointing at the old slot, which a different track now occupies.
#[test]
fn apply_sort_tracks_reanchors_the_playing_indicator_on_a_mixed_playlist() {
    use crate::config::{PlaylistColumnsConfig, SortBy};
    use crate::playlist::Playlist;
    use crate::stream::StreamKind;
    use crate::track::Track;
    use std::path::PathBuf;

    let mut app = App::new();
    // Filename strategy so the sort key is just the file stem (streams
    // fall back to the URL via stream_should_fallback_to_filename).
    let columns = PlaylistColumnsConfig {
        display_by: SortBy::Filename,
        ..PlaylistColumnsConfig::default()
    };

    // Build a queue that mirrors the user's bug report: a stream at
    // one end among several local files. Sort by Filename will swap
    // the stream's row with the locals; the indicator must follow.
    let stream_url = "https://www.youtube.com/watch?v=S_MOd40zlYU";
    let make_local = |stem: &str| {
        let mut t = Track::local(PathBuf::from(format!("/music/{stem}.mp3")));
        t.set_metadata(TrackMetadata {
            title: stem.to_string(),
            title_tagged: true,
            artist: String::new(),
            album: String::new(),
            track_number: None,
            duration: std::time::Duration::from_secs(60),
            bitrate: None,
            sample_rate: None,
            codec: String::new(),
            format: String::new(),
            album_artist: None,
            disc_number: None,
            genre: None,
            year: None,
            composer: None,
            comment: None,
        });
        t
    };
    let track_local_a = make_local("aaa");
    let track_local_b = make_local("bbb");
    let track_local_c = make_local("ccc");
    let track_stream = Track::from_stream(
        url::Url::parse(stream_url).expect("valid url"),
        StreamKind::Http,
    );

    app.state.playlist = {
        let mut pl = Playlist::new();
        pl.extend([track_local_a, track_local_b, track_local_c, track_stream]);
        pl
    };
    // Stream plays at index 3 (the tail of the queue).
    app.state.playback.track_index = Some(3);

    let _ = app.apply_sort_tracks(&columns);

    // After the sort, track_index must point to the row whose
    // the typed identity matches the stream URL — wherever the sort placed
    // it. Without the reanchor, track_index would still be 3, which
    // is now whichever local file happens to land there.
    let new_index = app
        .state()
        .playback
        .track_index
        .expect("track_index must remain set after a sort");
    let track_at_index = &app.state().playlist.tracks()[new_index];
    assert_eq!(
        track_at_index.display_location(),
        stream_url,
        "track_index must follow the stream after the sort; \
             the row at the old index would now be a different track"
    );
    assert!(
        track_at_index.is_stream(),
        "the row the indicator points at must be the stream"
    );
}
/// Settings changes the playlist-column presentation only. Reordering is
/// reserved for the separately confirmed Sort tracks action.
#[test]
fn apply_settings_under_metadata_does_not_reorder_the_playing_stream() {
    use crate::config::{PlaylistColumnsConfig, SortBy};
    use crate::playlist::Playlist;
    use crate::state::{Popup, SettingsDraft, SettingsFocus, SettingsTab};
    use crate::stream::StreamKind;
    use crate::track::Track;
    use std::path::PathBuf;

    let mut app = App::new();
    // Start with the stream at the tail of the queue and a stream
    // playing at that position.
    let stream_url = "https://www.youtube.com/watch?v=S_MOd40zlYU";
    app.state.playlist = {
        let mut pl = Playlist::new();
        pl.extend([
            Track::local(PathBuf::from("/music/aaa.mp3")),
            Track::local(PathBuf::from("/music/bbb.mp3")),
            Track::local(PathBuf::from("/music/ccc.mp3")),
            Track::from_stream(
                url::Url::parse(stream_url).expect("valid url"),
                StreamKind::Http,
            ),
        ]);
        pl
    };
    app.state.playback.track_index = Some(3);

    // Build the Settings draft by hand: a user changing from Filename to
    // Metadata. The queue must remain untouched while Settings closes.
    let themes_dir = std::path::PathBuf::new();
    let mut draft = SettingsDraft::from_state(&app.state, &app.config, &themes_dir);
    let columns = PlaylistColumnsConfig {
        display_by: SortBy::Metadata,
        ..PlaylistColumnsConfig::default()
    };
    draft.playlist_columns = columns.clone();
    draft.playlist_columns_initial = columns;
    app.state.popup_dialog.open_popup(Popup::Settings {
        tab: SettingsTab::General,
        focus: SettingsFocus::Content,
        draft,
    });

    let _ = app.apply_settings();

    // Applying presentation settings must not reorder or re-anchor the
    // queue. The stream remains at its stored position.
    let new_index = app
        .state()
        .playback
        .track_index
        .expect("track_index must remain set after Settings save");
    let track_at_index = &app.state().playlist.tracks()[new_index];
    assert!(
        track_at_index.is_stream(),
        "after Settings save, track_index must still point at the stream"
    );
    assert_eq!(
        track_at_index.display_location(),
        stream_url,
        "closing Settings must not move the playing stream"
    );
    assert_eq!(new_index, 3);
}

/// Repro for the exact user scenario: a stream is playing, the user
/// reorders with every configurable Metadata column selected (Track Number
/// + Artist + Album, with Title always on), and the ▶ indicator must follow the
///   stream to its new position. The previous tests only covered
///   Filename sort and a single Title-only Metadata sort; this one
///   pins the all-columns case the user reported.
#[test]
fn apply_sort_tracks_reanchors_when_all_metadata_fields_are_selected() {
    use crate::config::{PlaylistColumnsConfig, SortBy};
    use crate::playlist::Playlist;
    use crate::stream::StreamKind;
    use crate::track::Track;
    use std::path::PathBuf;

    let mut app = App::new();
    let columns = PlaylistColumnsConfig {
        display_by: SortBy::Metadata,
        metadata_track_number: true,
        metadata_artist: true,
        metadata_album: true,
    };

    // Local tracks carry full metadata so they all participate in
    // the sort under every field; the stream only carries Title,
    // so its sort key is just the title.
    let stream_url = "https://www.youtube.com/watch?v=S_MOd40zlYU";
    let make_local = |stem: &str, number: u32, artist: &str, album: &str, title: &str| {
        let mut t = Track::local(PathBuf::from(format!("/music/{stem}.mp3")));
        t.set_metadata(TrackMetadata {
            title: title.to_string(),
            title_tagged: true,
            artist: artist.to_string(),
            album: album.to_string(),
            track_number: Some(number),
            duration: std::time::Duration::from_secs(60),
            bitrate: None,
            sample_rate: None,
            codec: String::new(),
            format: String::new(),
            album_artist: None,
            disc_number: None,
            genre: None,
            year: None,
            composer: None,
            comment: None,
        });
        t
    };
    // Local titles that, when sorted under all-fields, end up
    // either before or after the stream — the bug fires
    // regardless of which side the stream lands on.
    let t1 = make_local("t1", 1, "Pearl Jam", "Ten", "Alive");
    let t2 = make_local("t2", 2, "Pearl Jam", "Ten", "Black");
    let t3 = make_local("t3", 3, "Pearl Jam", "Vs", "Animal");
    let t4 = make_local("t4", 4, "Pearl Jam", "Binaural", "Light Years");
    let stream = Track::from_stream(
        url::Url::parse(stream_url).expect("valid url"),
        StreamKind::Http,
    );
    // The stream keeps the title the resolver would attach.
    let mut stream = stream;
    stream.set_metadata(TrackMetadata {
        title: "Lofi Girl".to_string(),
        title_tagged: true,
        artist: String::new(),
        album: String::new(),
        track_number: None,
        duration: std::time::Duration::from_secs(60),
        bitrate: None,
        sample_rate: None,
        codec: String::new(),
        format: String::new(),
        album_artist: None,
        disc_number: None,
        genre: None,
        year: None,
        composer: None,
        comment: None,
    });

    app.state.playlist = {
        let mut pl = Playlist::new();
        pl.extend([t1, t2, t3, t4, stream]);
        pl
    };
    // Stream plays at index 4 (tail of the queue).
    app.state.playback.track_index = Some(4);
    // Cursor also on the stream so the panel highlight agrees.
    app.state.playlist.select(4);

    let _ = app.apply_sort_tracks(&columns);

    let new_index = app
        .state()
        .playback
        .track_index
        .expect("track_index must remain set after the sort");
    let track_at_index = &app.state().playlist.tracks()[new_index];
    assert!(
        track_at_index.is_stream(),
        "with all Metadata fields selected and a stream playing, \
             track_index must follow the stream to its new position; \
             the indicator at the new index points at {:?}",
        track_at_index.display_location()
    );
    assert_eq!(
        track_at_index.display_location(),
        stream_url,
        "the anchor must follow the stream's location, not stay at the pre-sort index"
    );
    // Cursor (panel highlight) must follow too, per the user's
    // observation that the focus is not on the playing track after
    // reordering when local tracks play.
    assert_eq!(
        app.state().playlist.cursor(),
        new_index,
        "the panel cursor must follow the playing track on sort"
    );
}

/// Repro for the user's exact flow: open Settings, flip the strategy
/// to Metadata with EVERY sub-option selected, save. The stream at
/// the tail of the queue must keep its ▶ marker and the cursor must
/// land on it, even when the locals (whose sort key is track number,
/// artist, album, title all populated) would push the stream away
/// from its pre-sort position.
#[test]
fn apply_settings_with_all_metadata_columns_does_not_reorder_the_stream() {
    use crate::config::{PlaylistColumnsConfig, SortBy};
    use crate::playlist::Playlist;
    use crate::state::{Popup, SettingsDraft, SettingsFocus, SettingsTab};
    use crate::stream::StreamKind;
    use crate::track::Track;
    use std::path::PathBuf;

    let mut app = App::new();
    let stream_url = "https://www.youtube.com/watch?v=S_MOd40zlYU";

    let make_local = |stem: &str, number: u32, artist: &str, album: &str, title: &str| {
        let mut t = Track::local(PathBuf::from(format!("/music/{stem}.mp3")));
        t.set_metadata(TrackMetadata {
            title: title.to_string(),
            title_tagged: true,
            artist: artist.to_string(),
            album: album.to_string(),
            track_number: Some(number),
            duration: std::time::Duration::from_secs(60),
            bitrate: None,
            sample_rate: None,
            codec: String::new(),
            format: String::new(),
            album_artist: None,
            disc_number: None,
            genre: None,
            year: None,
            composer: None,
            comment: None,
        });
        t
    };

    app.state.playlist = {
        let mut pl = Playlist::new();
        pl.extend([
            make_local("t1", 1, "Pearl Jam", "Ten", "Alive"),
            make_local("t2", 2, "Pearl Jam", "Ten", "Black"),
            make_local("t3", 3, "Pearl Jam", "Vs", "Animal"),
            make_local("t4", 4, "Pearl Jam", "Binaural", "Light Years"),
        ]);
        let mut stream = Track::from_stream(
            url::Url::parse(stream_url).expect("valid url"),
            StreamKind::Http,
        );
        stream.set_metadata(TrackMetadata {
            title: "Lofi Girl".to_string(),
            title_tagged: true,
            artist: String::new(),
            album: String::new(),
            track_number: None,
            duration: std::time::Duration::from_secs(60),
            bitrate: None,
            sample_rate: None,
            codec: String::new(),
            format: String::new(),
            album_artist: None,
            disc_number: None,
            genre: None,
            year: None,
            composer: None,
            comment: None,
        });
        pl.extend([stream]);
        pl
    };
    app.state.playback.track_index = Some(4);
    app.state.playlist.select(4);

    // Open Settings with the all-fields Metadata config in the draft.
    let themes_dir = std::path::PathBuf::new();
    let mut draft = SettingsDraft::from_state(&app.state, &app.config, &themes_dir);
    let all_fields = PlaylistColumnsConfig {
        display_by: SortBy::Metadata,
        metadata_track_number: true,
        metadata_artist: true,
        metadata_album: true,
    };
    draft.playlist_columns = all_fields.clone();
    draft.playlist_columns_initial = all_fields;
    app.state.popup_dialog.open_popup(Popup::Settings {
        tab: SettingsTab::General,
        focus: SettingsFocus::Content,
        draft,
    });

    let _ = app.apply_settings();
    assert!(
        app.active_popup().is_none(),
        "apply_settings must close the popup"
    );

    let new_index = app
        .state()
        .playback
        .track_index
        .expect("track_index must remain set after Settings save");
    let track_at_index = &app.state().playlist.tracks()[new_index];
    assert!(
        track_at_index.is_stream(),
        "the ▶ indicator must follow the stream under Metadata-all sort"
    );
    assert_eq!(
        track_at_index.display_location(),
        stream_url,
        "the anchor must follow the stream's location"
    );
    assert_eq!(
        app.state().playlist.cursor(),
        new_index,
        "the cursor must remain at the stored stream position"
    );
    assert_eq!(new_index, 4);
}

/// Regression test for the ▶ indicator landing on the wrong track
/// after a reorder while a stream plays. The bug lived in
/// `remap_stale_snapshot_index`, which matched the playing track by
/// `t.path()` — a field that is always `None` for streams, so the
/// remap returned the snapshot unchanged with the worker's stale
/// pre-reorder index. `apply_playback_progress` then overwrote the
/// reanchored track_index, leaving the indicator on whatever local
/// track the stale index now pointed at.
///
/// The fix captures the canonical location (URL for streams) in a
/// dedicated field and the remap compares against `t.track_location()`,
/// which works for both kinds. This test pins the contract end-to-end:
/// after a reorder + a stale snapshot, the worker's old index must
/// be remapped to the stream's new slot, not left in place.
#[test]
fn apply_playback_progress_remaps_stale_stream_index_after_reorder() {
    use crate::audio::PlaybackSnapshot;
    use crate::config::{PlaylistColumnsConfig, SortBy};
    use crate::playlist::Playlist;
    use crate::stream::StreamKind;
    use crate::track::Track;
    use std::path::PathBuf;

    let mut app = App::new();
    app.config.general.playlist_columns = PlaylistColumnsConfig {
        display_by: SortBy::Filename,
        ..PlaylistColumnsConfig::default()
    };

    let stream_url = "https://www.youtube.com/watch?v=S_MOd40zlYU";
    app.state.playlist = {
        let mut pl = Playlist::new();
        pl.extend([
            Track::local(PathBuf::from("/music/aaa.mp3")),
            Track::local(PathBuf::from("/music/bbb.mp3")),
            Track::local(PathBuf::from("/music/ccc.mp3")),
            Track::from_stream(
                url::Url::parse(stream_url).expect("valid url"),
                StreamKind::Http,
            ),
        ]);
        pl
    };

    // Begin playback of the stream: cursor must point at the stream
    // so `begin_current_track` picks it up and sets track_index = 3.
    // The typed identity stores the URL for both persistence and
    // snapshot-index remapping; the string form is derived at save time.
    app.state.playlist.select(3);
    let _ = app.begin_current_track();
    assert_eq!(app.state().playback.track_index, Some(3));
    assert_eq!(
        app.state().persistence.last_track,
        Some(TrackLocation::url(
            url::Url::parse(stream_url).expect("valid URL")
        )),
        "begin_current_track must capture the stream URL for identity"
    );
    assert_eq!(
        app.persisted_state().last_track_path.as_deref(),
        Some(stream_url),
        "stream persistence must use the stable URL"
    );

    // Reorder by Filename: locals come first (aaa < bbb < ccc), the
    // stream's URL falls back to the URL key (still last). The new
    // stream slot is now 3 too in this tiny fixture, so build a
    // second fixture where the stream actually moves position.
    let prefix = make_local_prefix("zz", "yy", "xx"); // force the stream off the tail
    let prefix_len = prefix.len();
    app.state.playlist = {
        let mut pl = Playlist::new();
        pl.extend(prefix);
        pl.extend([Track::from_stream(
            url::Url::parse(stream_url).expect("valid url"),
            StreamKind::Http,
        )]);
        pl
    };
    // Restore the playing-track identity across the rebuild.
    app.state.playback.track_index = Some(prefix_len);
    app.state.persistence.last_track = Some(TrackLocation::url(
        url::Url::parse(stream_url).expect("valid URL"),
    ));

    let columns = app.config.general.playlist_columns.clone();
    let _ = app.apply_sort_tracks(&columns);

    // After the sort, the reanchor has followed the stream to its
    // new position. Capture it before the worker snapshot arrives.
    let anchored_index = app
        .state()
        .playback
        .track_index
        .expect("track_index must remain set after the sort");

    // The audio worker keeps reporting the OLD (pre-sort) index
    // because it only knows about the path/URL it was handed. The
    // remap must translate it back to the stream's new position.
    let stale_snapshot = PlaybackSnapshot {
        track_index: Some(prefix_len),
        elapsed: std::time::Duration::from_millis(0),
        duration: None,
        status: crate::audio::PlayStatus::Playing,
        sink_health: crate::audio::SinkHealth::Healthy,
    };
    app.apply_playback_progress(stale_snapshot);

    assert_eq!(
        app.state().playback.track_index,
        Some(anchored_index),
        "stale stream snapshot must be remapped to the post-sort slot, \
             not overwrite the reanchored index with the pre-sort value"
    );
    let track_at_index =
        &app.state().playlist.tracks()[app.state().playback.track_index.expect("set")];
    assert_eq!(
        track_at_index.display_location(),
        stream_url,
        "after the remap the indicator must still land on the stream"
    );
}

/// Helper used by `apply_playback_progress_remaps_stale_stream_index_after_reorder`
/// to build a queue where the stream does NOT land at the tail under
/// the default Filename sort: prefixing with tracks whose stems sort
/// AFTER the URL keeps the stream at the end, which is what we want
/// to verify the indicator reanchor survives.
fn make_local_prefix(a: &str, b: &str, c: &str) -> Vec<crate::track::Track> {
    use crate::track::Track;
    vec![
        Track::local(std::path::PathBuf::from(format!("/music/{a}.mp3"))),
        Track::local(std::path::PathBuf::from(format!("/music/{b}.mp3"))),
        Track::local(std::path::PathBuf::from(format!("/music/{c}.mp3"))),
    ]
}

#[test]
fn metadata_apply_does_not_reorder_under_filename_sort() {
    use crate::playlist::Playlist;
    use crate::track::Track;

    let mut app = App::new();
    // Filename is the default strategy; metadata arrival must not
    // reorder entries whose stems are already in order.
    assert_eq!(
        app.config.general.playlist_columns.display_by,
        crate::config::SortBy::Filename
    );
    let path_first = PathBuf::from("/music/a1.mp3");
    let path_second = PathBuf::from("/music/a2.mp3");
    app.state.playlist = {
        let mut pl = Playlist::new();
        pl.extend([
            Track::local(path_first.clone()),
            Track::local(path_second.clone()),
        ]);
        pl
    };

    let metadata_with_track_number = |number| TrackMetadata {
        title: format!("Track {number}"),
        title_tagged: true,
        artist: "A".to_string(),
        album: "B".to_string(),
        track_number: Some(number),
        duration: std::time::Duration::from_secs(60),
        bitrate: None,
        sample_rate: None,
        codec: String::new(),
        format: String::new(),
        album_artist: None,
        disc_number: None,
        genre: None,
        year: None,
        composer: None,
        comment: None,
    };
    app.apply_metadata_completed(
        vec![
            (path_first.clone(), metadata_with_track_number(2)),
            (path_second.clone(), metadata_with_track_number(1)),
        ],
        0,
    );

    let ordered: Vec<String> = app
        .state()
        .playlist
        .tracks()
        .iter()
        .map(|t| t.display_location().into_owned())
        .collect();
    assert_eq!(
        ordered,
        vec!["/music/a1.mp3", "/music/a2.mp3"],
        "Filename strategy must ignore the metadata order"
    );
}

#[test]
fn explicit_sort_keeps_the_playing_track_after_metadata_arrival() {
    use crate::config::{PlaylistColumnsConfig, SortBy};
    use crate::playlist::Playlist;
    use crate::track::Track;

    let mut app = App::new();
    let columns = PlaylistColumnsConfig {
        display_by: SortBy::Metadata,
        metadata_track_number: true,
        ..PlaylistColumnsConfig::default()
    };
    app.state.playlist_columns = columns.clone();
    let path_first = PathBuf::from("/music/track1.mp3");
    let path_second = PathBuf::from("/music/track2.mp3");
    app.state.playlist = {
        let mut pl = Playlist::new();
        pl.extend([
            Track::local(path_first.clone()),
            Track::local(path_second.clone()),
        ]);
        pl
    };
    // The second entry (track 2) is playing and holds the cursor.
    app.state.playback.track_index = Some(1);
    app.state.playlist.select(1);

    let metadata_with_track_number = |number| TrackMetadata {
        title: format!("Track {number}"),
        title_tagged: true,
        artist: "A".to_string(),
        album: "B".to_string(),
        track_number: Some(number),
        duration: std::time::Duration::from_secs(60),
        bitrate: None,
        sample_rate: None,
        codec: String::new(),
        format: String::new(),
        album_artist: None,
        disc_number: None,
        genre: None,
        year: None,
        composer: None,
        comment: None,
    };
    app.apply_metadata_completed(
        vec![
            (path_first.clone(), metadata_with_track_number(2)),
            (path_second.clone(), metadata_with_track_number(1)),
        ],
        0,
    );

    assert_eq!(
        app.state().playlist.tracks()[0].display_location(),
        "/music/track1.mp3"
    );
    assert_eq!(
        app.state().playback.track_index,
        Some(1),
        "metadata arrival must not move the playing track"
    );
    assert_eq!(
        app.state().playlist.cursor(),
        1,
        "metadata arrival must not move the cursor"
    );

    let effects = app.apply_sort_tracks(&columns);
    assert!(effects.is_empty());
    assert_eq!(
        app.state().playlist.tracks()[0].display_location(),
        "/music/track2.mp3"
    );
    assert_eq!(
        app.state().playback.track_index,
        Some(0),
        "the explicit sort must move the playing track by identity"
    );
    assert_eq!(app.state().playlist.cursor(), 0);
}

/// Fixture tree: one marked subdirectory plus a supported audio file.
fn add_fixture() -> (App, TestTempDir) {
    let root = unique_temp_dir("add-selected");
    fs::create_dir_all(root.join("Album")).expect("dir");
    fs::write(root.join("song.mp3"), "").expect("file");
    fs::write(root.join("notes.txt"), "").expect("file");

    let mut app = App::new();
    app.state.change_browser_dir(
        root.to_path_buf(),
        crate::filesystem::read_sorted_entries(&root, false).expect("browser loads fixture"),
    );

    (app, root)
}

#[test]
fn add_selected_marks_a_directory_and_yields_one_scan_effect() {
    let (mut app, root) = add_fixture();
    let album = root.join("Album");

    let index = app
        .state()
        .browser
        .entries
        .iter()
        .position(|entry| entry.path == album)
        .expect("album listed");
    app.state_mut().browser.set_cursor(index);
    // One state borrow feeds both sides, the fields are disjoint
    let state = app.state_mut();
    let mut selected = std::mem::take(&mut state.browser.selected_entries);
    state.browser.toggle_mark(&mut selected);
    state.browser.selected_entries = selected;

    let effects = app.handle_command(Command::AddSelected);

    assert_eq!(effects, vec![Effect::ScanDirectory(album.clone())]);
    // Consumption semantics cleared the marks after taking them
    assert!(app.state().browser.selected_entries.is_empty());
    // Directories never join the playlist synchronously
    assert!(app.state().playlist.is_empty());
}

#[test]
fn add_selected_pushes_marked_files_directly_into_the_playlist() {
    let (mut app, root) = add_fixture();

    let index = app
        .state()
        .browser
        .entries
        .iter()
        .position(|entry| entry.name == "song.mp3")
        .expect("audio listed");
    app.state_mut().browser.set_cursor(index);
    // One state borrow feeds both sides, the fields are disjoint
    let state = app.state_mut();
    let mut selected = std::mem::take(&mut state.browser.selected_entries);
    state.browser.toggle_mark(&mut selected);
    state.browser.selected_entries = selected;

    let effects = app.handle_command(Command::AddSelected);

    assert_eq!(app.state().playlist.len(), 1);
    assert_eq!(
        app.state().playlist.tracks()[0].path(),
        Some(root.join("song.mp3").as_path())
    );
    // Queued files immediately request tag extraction
    assert_eq!(
        effects,
        vec![Effect::LoadMetadata(vec![root.join("song.mp3")])]
    );
}

#[test]
fn activating_an_unsupported_file_notifies_without_queueing() {
    let (mut app, _root) = add_fixture();

    let index = app
        .state()
        .browser
        .entries
        .iter()
        .position(|entry| entry.name == "notes.txt")
        .expect("text listed");
    app.state_mut().browser.set_cursor(index);

    let effects = app.handle_command(Command::EnterSelected);

    assert!(effects.is_empty());
    assert!(app.state().playlist.is_empty());
    assert_eq!(
        app.state().notifications.last(),
        Some(&"Unsupported file type: notes.txt".to_string())
    );
}

#[test]
fn entering_a_directory_changes_location_and_clears_marks() {
    let (mut app, root) = add_fixture();
    let album = root.join("Album");
    app.state_mut()
        .browser
        .selected_entries
        .insert(PathBuf::from("/stale.mp3"));

    let index = app
        .state()
        .browser
        .entries
        .iter()
        .position(|entry| entry.path == album)
        .expect("album listed");
    app.state_mut().browser.set_cursor(index);

    let effects = app.handle_command(Command::EnterSelected);

    let Effect::LoadBrowserDirectory {
        request_id,
        dir,
        restore_cursor_name,
        ..
    } = effects.into_iter().next().expect("directory effect")
    else {
        panic!("expected a browser directory effect");
    };
    let entries = crate::filesystem::read_sorted_entries(&dir, false).expect("listing");
    let _ = app.apply_browser_directory_loaded(request_id, dir, entries, restore_cursor_name);
    assert_eq!(app.state().browser.current_dir, album);
    assert!(app.state().browser.selected_entries.is_empty());
}

#[test]
fn cursor_is_restored_on_the_folder_being_left_without_a_descent() {
    let (mut app, root) = add_fixture();
    let album = root.join("Album");

    // Simulate arriving in Album via the persisted start directory (no
    // descent happened, so the focus stack is empty).
    app.state.change_browser_dir(
        album.clone(),
        crate::filesystem::read_sorted_entries(&album, false).expect("browser loads album"),
    );

    // Ascend: the cursor must land on "Album" in the parent listing, not
    // on the first entry (which is a file in this fixture).
    let effects = app.handle_command(Command::ParentDir);
    let Effect::LoadBrowserDirectory {
        request_id,
        dir,
        restore_cursor_name,
        ..
    } = effects.into_iter().next().expect("parent directory effect")
    else {
        panic!("expected a parent directory effect");
    };
    let entries = crate::filesystem::read_sorted_entries(&dir, false).expect("listing");
    let _ = app.apply_browser_directory_loaded(request_id, dir, entries, restore_cursor_name);

    assert_eq!(app.state().browser.current_dir, root.path());
    let cursor_name = app
        .state()
        .browser
        .entries
        .get(app.state().browser.cursor())
        .map(|entry| entry.name.clone())
        .unwrap_or_default();
    assert_eq!(
        cursor_name, "Album",
        "the cursor must return to the folder that was just left"
    );
}

#[test]
fn coming_back_up_places_the_cursor_on_the_folder_that_was_left() {
    let (mut app, root) = add_fixture();
    let album = root.join("Album");

    let index = app
        .state()
        .browser
        .entries
        .iter()
        .position(|entry| entry.path == album)
        .expect("album listed");
    app.state_mut().browser.set_cursor(index);
    let effects = app.handle_command(Command::EnterSelected); // descend into Album
    let Effect::LoadBrowserDirectory {
        request_id,
        dir,
        restore_cursor_name,
        ..
    } = effects.into_iter().next().expect("directory effect")
    else {
        panic!("expected a browser directory effect");
    };
    let entries = crate::filesystem::read_sorted_entries(&dir, false).expect("listing");
    let _ = app.apply_browser_directory_loaded(request_id, dir, entries, restore_cursor_name);

    assert_eq!(app.state().browser.current_dir, album);
    let effects = app.handle_command(Command::ParentDir); // back up to root
    let Effect::LoadBrowserDirectory {
        request_id,
        dir,
        restore_cursor_name,
        ..
    } = effects.into_iter().next().expect("parent directory effect")
    else {
        panic!("expected a parent directory effect");
    };
    let entries = crate::filesystem::read_sorted_entries(&dir, false).expect("listing");
    let _ = app.apply_browser_directory_loaded(request_id, dir, entries, restore_cursor_name);

    assert_eq!(app.state().browser.current_dir, root.path());
    let cursor_name = app
        .state()
        .browser
        .entries
        .get(app.state().browser.cursor())
        .map(|entry| entry.name.clone())
        .expect("cursor lands on an entry");
    assert_eq!(
        cursor_name, "Album",
        "the cursor returns to the folder we just left"
    );
}

#[test]
fn cursor_is_restored_on_the_correct_folder_at_every_level() {
    let (mut app, root) = add_fixture();
    let album = root.join("Album");
    // Create a nested folder inside Album so we can descend twice.
    let nested = album.join("Nested");
    fs::create_dir_all(&nested).expect("nested dir");

    // Descend into Album.
    let index = app
        .state()
        .browser
        .entries
        .iter()
        .position(|entry| entry.path == album)
        .expect("album listed");
    app.state_mut().browser.set_cursor(index);
    let effects = app.handle_command(Command::EnterSelected);
    let Effect::LoadBrowserDirectory {
        request_id,
        dir,
        restore_cursor_name,
        ..
    } = effects.into_iter().next().expect("Album directory effect")
    else {
        panic!("expected an Album directory effect");
    };
    let entries = crate::filesystem::read_sorted_entries(&dir, false).expect("Album listing");
    let _ = app.apply_browser_directory_loaded(request_id, dir, entries, restore_cursor_name);
    assert_eq!(app.state().browser.current_dir, album);

    // Descend into Nested.
    let index = app
        .state()
        .browser
        .entries
        .iter()
        .position(|entry| entry.path == nested)
        .expect("nested listed");
    app.state_mut().browser.set_cursor(index);
    let effects = app.handle_command(Command::EnterSelected);
    let Effect::LoadBrowserDirectory {
        request_id,
        dir,
        restore_cursor_name,
        ..
    } = effects.into_iter().next().expect("Nested directory effect")
    else {
        panic!("expected a Nested directory effect");
    };
    let entries = crate::filesystem::read_sorted_entries(&dir, false).expect("Nested listing");
    let _ = app.apply_browser_directory_loaded(request_id, dir, entries, restore_cursor_name);
    assert_eq!(app.state().browser.current_dir, nested);

    // Up once: cursor on "Nested" inside Album.
    let effects = app.handle_command(Command::ParentDir);
    let Effect::LoadBrowserDirectory {
        request_id,
        dir,
        restore_cursor_name,
        ..
    } = effects.into_iter().next().expect("parent directory effect")
    else {
        panic!("expected a parent directory effect");
    };
    let entries = crate::filesystem::read_sorted_entries(&dir, false).expect("parent listing");
    let _ = app.apply_browser_directory_loaded(request_id, dir, entries, restore_cursor_name);
    assert_eq!(app.state().browser.current_dir, album);
    let name = app
        .state()
        .browser
        .entries
        .get(app.state().browser.cursor())
        .map(|e| e.name.clone())
        .unwrap_or_default();
    assert_eq!(name, "Nested", "level 1: cursor restored on Nested");

    // Up twice: cursor on "Album" inside root.
    let effects = app.handle_command(Command::ParentDir);
    let Effect::LoadBrowserDirectory {
        request_id,
        dir,
        restore_cursor_name,
        ..
    } = effects.into_iter().next().expect("parent directory effect")
    else {
        panic!("expected a parent directory effect");
    };
    let entries = crate::filesystem::read_sorted_entries(&dir, false).expect("parent listing");
    let _ = app.apply_browser_directory_loaded(request_id, dir, entries, restore_cursor_name);
    assert_eq!(app.state().browser.current_dir, root.path());
    let name = app
        .state()
        .browser
        .entries
        .get(app.state().browser.cursor())
        .map(|e| e.name.clone())
        .unwrap_or_default();
    assert_eq!(name, "Album", "level 2: cursor restored on Album");
}

#[test]
fn parent_navigation_from_the_fixture_root_climbs_one_level() {
    let (mut app, root) = add_fixture();

    let effects = app.handle_command(Command::ParentDir);

    let Effect::LoadBrowserDirectory {
        request_id,
        dir,
        restore_cursor_name,
        ..
    } = effects.into_iter().next().expect("parent directory effect")
    else {
        panic!("expected a parent directory effect");
    };
    let entries = crate::filesystem::read_sorted_entries(&dir, false).expect("parent listing");
    let _ = app.apply_browser_directory_loaded(request_id, dir, entries, restore_cursor_name);
    assert_eq!(
        app.state().browser.current_dir,
        root.parent().expect("tmp parent")
    );
}

#[test]
fn full_add_flow_spawns_a_scan_and_metadata_observable_on_the_bus() {
    let (mut app, root) = add_fixture();
    let album = root.join("Album");
    fs::write(album.join("one.mp3"), "").expect("track one");
    fs::write(album.join("two.flac"), "").expect("track two");

    let index = app
        .state()
        .browser
        .entries
        .iter()
        .position(|entry| entry.path == album)
        .expect("album listed");
    app.state_mut().browser.set_cursor(index);

    let effects = app.handle_command(Command::AddSelected);
    assert_eq!(effects.len(), 1, "one directory means one scan task");

    let services = AppServices::new().expect("services construction");
    execute_effects(effects, &services, &pending_counter());

    let completed = match services.events().recv_timeout(EVENT_WAIT) {
        Ok(AppEvent::ScanCompleted {
            operation_id: _,
            requested_dir,
            tracks,
        }) => {
            assert_eq!(requested_dir, album);
            tracks
        }
        Ok(event) => panic!("expected ScanCompleted, got {event:?}"),
        Err(RecvTimeoutError::Timeout) => panic!("timed out waiting for the scan"),
        Err(error) => panic!("bus receive failed: {error}"),
    };

    let follow_ups = app.apply_scan_completed(album, completed);
    assert_eq!(follow_ups.len(), 1, "queued paths request extraction");
    assert_eq!(app.state().playlist.len(), 2);

    execute_effects(follow_ups, &services, &pending_counter());
    match services.events().recv_timeout(EVENT_WAIT) {
        Ok(AppEvent::MetadataCompleted {
            operation_id: _,
            loaded,
            failed,
        }) => {
            // The fixtures are empty files, so extraction must fail
            // gracefully for both instead of crashing the worker
            assert!(loaded.is_empty());
            assert_eq!(failed, 2);
        }
        Ok(event) => panic!("expected MetadataCompleted, got {event:?}"),
        Err(RecvTimeoutError::Timeout) => panic!("timed out waiting for metadata"),
        Err(error) => panic!("bus receive failed: {error}"),
    }

    services.shutdown();
}

#[test]
fn edit_metadata_prefill_publishes_raw_fields_on_the_bus() {
    let root = unique_temp_dir("app-prefill");
    let path = root.join("song.wav");
    fs::write(
        &path,
        crate::test_support::wav_bytes(&[("INAM", "Tom Sawyer")]),
    )
    .expect("fixture");

    let effects = vec![Effect::EditMetadataPrefill { path: path.clone() }];
    let services = AppServices::new().expect("services construction");
    execute_effects(effects, &services, &pending_counter());

    match services.events().recv_timeout(EVENT_WAIT) {
        Ok(AppEvent::MetadataPrefillReady {
            operation_id: _,
            path: got,
            fields,
        }) => {
            assert_eq!(got, path);
            assert_eq!(fields[0], "Tom Sawyer", "title prefill");
            assert!(
                fields[1..].iter().all(|field| field.is_empty()),
                "only the title tag exists, got {fields:?}"
            );
        }
        Ok(event) => panic!("expected MetadataPrefillReady, got {event:?}"),
        Err(RecvTimeoutError::Timeout) => panic!("timed out waiting for the prefill"),
        Err(error) => panic!("bus receive failed: {error}"),
    }

    services.shutdown();
}

#[test]
fn rename_file_opens_preloaded_with_the_cursor_file_name() {
    let root = unique_temp_dir("app-rename-open");
    fs::write(root.join("track01.flac"), b"audio").expect("fixture");

    let mut app = App::new();
    app.state_mut().change_browser_dir(
        root.to_path_buf(),
        crate::filesystem::read_sorted_entries(&root, false).expect("listing"),
    );

    app.handle_command(Command::RenameFile);

    match app.state().popup_dialog.dialog_mode_ref() {
        Some(DialogMode::RenameFile {
            path,
            original_name,
            error,
        }) => {
            assert_eq!(path, &root.join("track01.flac"));
            assert_eq!(original_name, "track01.flac");
            assert_eq!(error, &None);
        }
        other => panic!("expected RenameFile dialog, got {other:?}"),
    }
    assert_eq!(
        app.state().popup_dialog.dialog_input_value(),
        "track01",
        "only the base name is editable"
    );
    assert_eq!(
        app.state().popup_dialog.dialog_extension_value(),
        Some(".flac"),
        "the extension is locked separately"
    );
    assert_eq!(
        app.state().popup_dialog.dialog_cursor_value(),
        "track01".chars().count(),
        "the cursor lands at the end of the editable base"
    );
}

#[test]
fn rename_file_cursor_walks_with_left_right_and_home_end() {
    let mut app = App::new();
    app.state.popup_dialog.open_dialog(
        DialogMode::RenameFile {
            path: PathBuf::from("/music/song.wav"),
            original_name: "song.wav".to_string(),
            error: None,
        },
        "song.wav".to_string(),
        None,
    );

    let press = |code| KeyEvent::new(code, KeyModifiers::NONE);

    app.handle_dialog_key_event(press(KeyCode::Left));
    assert_eq!(
        app.state.popup_dialog.dialog_cursor_value(),
        "song.wav".chars().count() - 1,
        "Left pulls the cursor one character back"
    );
    app.handle_dialog_key_event(press(KeyCode::Left));
    app.handle_dialog_key_event(press(KeyCode::Left));
    assert_eq!(
        app.state.popup_dialog.dialog_cursor_value(),
        "song.wav".chars().count() - 3,
        "repeated Left walks back further"
    );

    // Walking off the start clamps to zero, never underflowing
    for _ in 0..20 {
        app.handle_dialog_key_event(press(KeyCode::Left));
    }
    assert_eq!(
        app.state.popup_dialog.dialog_cursor_value(),
        0,
        "Left clamps at the start"
    );

    app.handle_dialog_key_event(press(KeyCode::End));
    assert_eq!(
        app.state.popup_dialog.dialog_cursor_value(),
        "song.wav".chars().count(),
        "End jumps back to the tail"
    );
    app.handle_dialog_key_event(press(KeyCode::Home));
    assert_eq!(
        app.state.popup_dialog.dialog_cursor_value(),
        0,
        "Home jumps to the start"
    );

    // Right past the end clamps to the buffer length
    for _ in 0..20 {
        app.handle_dialog_key_event(press(KeyCode::Right));
    }
    assert_eq!(
        app.state.popup_dialog.dialog_cursor_value(),
        "song.wav".chars().count(),
        "Right clamps at the end"
    );
}

#[test]
fn rename_file_backspace_and_delete_edit_around_the_cursor() {
    let mut app = App::new();
    app.state.popup_dialog.open_dialog(
        DialogMode::RenameFile {
            path: PathBuf::from("/music/song.wav"),
            original_name: "song.wav".to_string(),
            error: None,
        },
        "song.wav".to_string(),
        None,
    );
    // Start the cursor in the middle, between "song" and ".wav"
    let mid = "song".chars().count();
    app.state.popup_dialog.set_dialog_cursor(mid);

    let press = |code| KeyEvent::new(code, KeyModifiers::NONE);

    app.handle_dialog_key_event(press(KeyCode::Backspace));
    assert_eq!(
        app.state.popup_dialog.dialog_input_value(),
        "son.wav",
        "Backspace drops the char before the cursor"
    );
    assert_eq!(
        app.state.popup_dialog.dialog_cursor_value(),
        mid - 1,
        "Backspace also pulls the cursor back"
    );

    // Delete at the cursor drops the char to the right without moving
    app.handle_dialog_key_event(press(KeyCode::Delete));
    assert_eq!(
        app.state.popup_dialog.dialog_input_value(),
        "sonwav",
        "Delete drops the char at the cursor (the dot)"
    );
    assert_eq!(
        app.state.popup_dialog.dialog_cursor_value(),
        mid - 1,
        "Delete leaves the cursor in place"
    );

    // Insertion lands at the cursor and pushes later characters to the right
    app.handle_dialog_key_event(press(KeyCode::Char('.')));
    assert_eq!(
        app.state.popup_dialog.dialog_input_value(),
        "son.wav",
        "the char is inserted at the cursor"
    );
    assert_eq!(
        app.state.popup_dialog.dialog_cursor_value(),
        mid,
        "the cursor advances past the inserted char"
    );
}

#[test]
fn rename_file_arrow_keys_never_break_utf8_boundaries() {
    let mut app = App::new();
    app.state.popup_dialog.open_dialog(
        DialogMode::RenameFile {
            path: PathBuf::from("/music/canción.wav"),
            original_name: "canción.wav".to_string(),
            error: None,
        },
        "canción.wav".to_string(),
        None,
    );
    let total = "canción.wav".chars().count();
    app.state.popup_dialog.set_dialog_cursor(total);

    let press = |code| KeyEvent::new(code, KeyModifiers::NONE);

    // Walk back over the multibyte "ó" (one character) and confirm the
    // buffer remains valid UTF-8
    app.handle_dialog_key_event(press(KeyCode::Left));
    assert!(std::str::from_utf8(app.state.popup_dialog.dialog_input_value().as_bytes()).is_ok());
    assert_eq!(
        app.state.popup_dialog.dialog_cursor_value(),
        total - 1,
        "the cursor moves one character"
    );

    // Insert at the end pushes the trailing 'v' to the right while the
    // multibyte "ó" remains a single grapheme boundary away
    app.handle_dialog_key_event(press(KeyCode::Char('o')));
    assert_eq!(
        app.state.popup_dialog.dialog_input_value(),
        "canción.waov",
        "Insertion lands on a UTF-8 boundary without breaking the multibyte sequence"
    );
    assert!(std::str::from_utf8(app.state.popup_dialog.dialog_input_value().as_bytes()).is_ok());
}

#[test]
fn rename_on_a_non_audio_entry_shows_a_notification_only() {
    let root = unique_temp_dir("app-rename-nonaudio");
    fs::write(root.join("notes.txt"), b"text").expect("fixture");

    let mut app = App::new();
    app.state_mut().change_browser_dir(
        root.to_path_buf(),
        crate::filesystem::read_sorted_entries(&root, false).expect("listing"),
    );

    app.handle_command(Command::RenameFile);

    assert!(
        app.state().popup_dialog.dialog_mode_ref().is_none(),
        "no dialog for text files"
    );
    assert!(
        app.state()
            .notifications
            .iter()
            .any(|message| message.contains("rename")),
        "the rejection must be visible"
    );
}

#[test]
fn rename_collision_alert_dismisses_back_to_the_editable_dialog() {
    let mut app = App::new();
    app.state.popup_dialog.open_dialog(
        DialogMode::RenameFile {
            path: PathBuf::from("/music/song.wav"),
            original_name: "song.wav".to_string(),
            error: None,
        },
        "solo.wav".to_string(),
        None,
    );
    app.state.popup_dialog.push_popup(Popup::RenameCollision {
        existing: PathBuf::from("/music/solo.wav"),
        attempted: "solo.wav".to_string(),
    });

    // The alert owns the screen: Esc dismisses it and the rename dialog
    // underneath stays open and editable
    let effects = app.handle_key_event(esc_key());

    assert!(effects.is_empty());
    assert!(
        app.state().popup_dialog.active_popup_ref().is_none(),
        "alert dismissed"
    );
    assert!(
        matches!(
            app.state().popup_dialog.dialog_mode_ref(),
            Some(DialogMode::RenameFile { .. })
        ),
        "the dialog must stay open"
    );
    assert_eq!(
        app.state().popup_dialog.dialog_input_value(),
        "solo.wav",
        "still editable"
    );
}

#[test]
fn edit_metadata_opens_loading_with_empty_fields_and_a_prefill_effect() {
    let root = unique_temp_dir("app-metadata-open");
    fs::write(root.join("song.wav"), crate::test_support::wav_bytes(&[])).expect("fixture");

    let mut app = App::new();
    app.state_mut().change_browser_dir(
        root.to_path_buf(),
        crate::filesystem::read_sorted_entries(&root, false).expect("listing"),
    );

    let effects = app.handle_command(Command::EditMetadata);

    match app.state().popup_dialog.dialog_mode_ref() {
        Some(DialogMode::EditMetadata {
            path,
            fields,
            cursor,
            loading,
            ..
        }) => {
            assert_eq!(path, &root.join("song.wav"));
            assert_eq!(cursor, &0);
            assert!(loading, "the form waits for the prefill");
            assert!(
                fields.iter().all(|field| field.is_empty()),
                "placeholder fields must be empty"
            );
        }
        other => panic!("expected EditMetadata dialog, got {other:?}"),
    }
    assert_eq!(
        effects,
        vec![Effect::EditMetadataPrefill {
            path: root.join("song.wav")
        }]
    );
}

#[test]
fn prefill_ready_fills_the_form_when_the_dialog_matches() {
    let path = PathBuf::from("/music/song.wav");
    let mut app = App::new();
    app.state.popup_dialog.open_dialog(
        DialogMode::EditMetadata {
            path: path.clone(),
            fields: Default::default(),
            cursor: 0,
            error: None,
            loading: true,
        },
        String::new(),
        None,
    );

    let mut fields: Box<[String; 10]> = Default::default();
    fields[0] = "Tom Sawyer".to_string();
    fields[1] = "Rush".to_string();
    fields[7] = "1981".to_string();
    app.apply_metadata_prefill_ready(path.clone(), (*fields).clone());

    match app.state().popup_dialog.dialog_mode_ref() {
        Some(DialogMode::EditMetadata {
            path: got,
            fields: stored,
            loading,
            ..
        }) => {
            assert_eq!(got, &path);
            assert_eq!(stored, &fields);
            assert!(!loading, "prefill clears the loading state");
        }
        other => panic!("expected EditMetadata dialog, got {other:?}"),
    }
    assert_eq!(
        app.state().popup_dialog.dialog_input_value(),
        "Tom Sawyer",
        "first field buffer"
    );
}

#[test]
fn prefill_for_another_path_is_dropped_by_the_staleness_gate() {
    let mut app = App::new();
    app.state.popup_dialog.open_dialog(
        DialogMode::EditMetadata {
            path: PathBuf::from("/music/song.wav"),
            fields: Default::default(),
            cursor: 0,
            error: None,
            loading: true,
        },
        String::new(),
        None,
    );

    let mut fields: [String; 10] = Default::default();
    fields[0] = "Late".to_string();
    app.apply_metadata_prefill_ready(PathBuf::from("/music/other.wav"), fields);

    match app.state().popup_dialog.dialog_mode_ref() {
        Some(DialogMode::EditMetadata { loading, .. }) => {
            assert!(loading, "a late delivery must not touch the form");
        }
        other => panic!("expected EditMetadata dialog, got {other:?}"),
    }
}

#[test]
fn metadata_form_enter_saves_every_field_and_closes_the_popup() {
    let path = PathBuf::from("/music/song.wav");
    let mut app = App::new();
    let mut prefill: [String; 10] = Default::default();
    prefill[1] = "Rush".to_string();
    prefill[7] = "1981".to_string();
    app.state.popup_dialog.open_dialog(
        DialogMode::EditMetadata {
            path: path.clone(),
            fields: Box::new(prefill.clone()),
            cursor: 0,
            error: None,
            loading: false,
        },
        "New Title".to_string(),
        None,
    );

    let effects = app.handle_dialog_key_event(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

    assert!(
        app.state().popup_dialog.dialog_mode_ref().is_none(),
        "Enter commits the form and closes it"
    );
    let mut expected = prefill.clone();
    expected[0] = "New Title".to_string();
    assert_eq!(
        effects,
        vec![Effect::EditMetadataWrite {
            path,
            fields: expected
        }],
        "Enter dispatches the write with every field, including the unchanged ones"
    );
}

#[test]
fn metadata_form_arrow_keys_move_between_fields_without_advancing_on_enter() {
    let mut app = App::new();
    let mut prefill: [String; 10] = Default::default();
    prefill[0] = "Old Title".to_string();
    app.state.popup_dialog.open_dialog(
        DialogMode::EditMetadata {
            path: PathBuf::from("/music/song.wav"),
            fields: Box::new(prefill),
            cursor: 0,
            error: None,
            loading: false,
        },
        "Old Title".to_string(),
        None,
    );

    // Down moves the focus to field 1, committing the draft for field 0
    app.handle_dialog_key_event(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
    match app.state().popup_dialog.dialog_mode_ref() {
        Some(DialogMode::EditMetadata { fields, cursor, .. }) => {
            assert_eq!(*cursor, 1, "Down moved to the next field");
            assert_eq!(fields[0], "Old Title", "field 0 kept its draft");
            assert_eq!(fields[1], "", "field 1 stays empty");
        }
        other => panic!("expected EditMetadata dialog, got {other:?}"),
    }
    assert_eq!(
        app.state().popup_dialog.dialog_input_value(),
        "",
        "buffer reloaded from the new field"
    );

    // Up returns to field 0
    app.handle_dialog_key_event(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
    match app.state().popup_dialog.dialog_mode_ref() {
        Some(DialogMode::EditMetadata { cursor, .. }) => {
            assert_eq!(*cursor, 0, "Up returned to the previous field");
        }
        other => panic!("expected EditMetadata dialog, got {other:?}"),
    }
    assert_eq!(
        app.state().popup_dialog.dialog_input_value(),
        "Old Title",
        "buffer reloaded the focused field value"
    );
}

#[test]
fn metadata_form_enter_on_the_last_field_dispatches_the_save() {
    let path = PathBuf::from("/music/song.wav");
    let mut app = App::new();
    app.state.popup_dialog.open_dialog(
        DialogMode::EditMetadata {
            path: path.clone(),
            fields: Default::default(),
            cursor: 9,
            error: None,
            loading: false,
        },
        "Final comment".to_string(),
        None,
    );

    let effects = app.handle_dialog_key_event(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

    assert!(
        app.state().popup_dialog.dialog_mode_ref().is_none(),
        "save closes the form"
    );
    let mut fields: [String; 10] = Default::default();
    fields[9] = "Final comment".to_string();
    assert_eq!(effects, vec![Effect::EditMetadataWrite { path, fields }]);
}

#[test]
fn metadata_form_escape_discards_everything_without_writing() {
    let mut app = App::new();
    app.state.popup_dialog.open_dialog(
        DialogMode::EditMetadata {
            path: PathBuf::from("/music/song.wav"),
            fields: Default::default(),
            cursor: 0,
            error: None,
            loading: false,
        },
        "pending draft".to_string(),
        None,
    );

    let effects = app.handle_dialog_key_event(esc_key());

    assert!(effects.is_empty(), "Esc never writes");
    assert!(
        app.state().popup_dialog.dialog_mode_ref().is_none(),
        "form closed"
    );
    assert_eq!(app.state().popup_dialog.dialog_input_value(), "");
}

#[test]
fn metadata_form_ignores_input_while_the_prefill_is_loading() {
    let mut app = App::new();
    app.state.popup_dialog.open_dialog(
        DialogMode::EditMetadata {
            path: PathBuf::from("/music/song.wav"),
            fields: Default::default(),
            cursor: 0,
            error: None,
            loading: true,
        },
        String::new(),
        None,
    );

    let effects =
        app.handle_dialog_key_event(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE));

    assert!(effects.is_empty());
    assert_eq!(
        app.state().popup_dialog.dialog_input_value(),
        "",
        "no input before the prefill"
    );
    assert!(
        matches!(
            app.state().popup_dialog.dialog_mode_ref(),
            Some(DialogMode::EditMetadata { loading: true, .. })
        ),
        "the form stays loading"
    );
}

#[test]
fn rename_completed_rewrites_the_queue_and_keeps_playback_index() {
    // Queue built directly so the same path can appear twice, covering
    // the duplicate-entry rule (extend_playlist deduplicates on purpose)
    let mut app = App::new();
    app.state.playlist.extend([
        crate::track::Track::local("/a.mp3"),
        crate::track::Track::local("/b.mp3"),
        crate::track::Track::local("/c.mp3"),
        crate::track::Track::local("/b.mp3"),
    ]);
    app.state.playlist.select(1);
    app.state.playback.track_index = Some(1);
    app.state.persistence.last_track = Some(TrackLocation::local("/b.mp3"));

    let effects = app.apply_rename_completed(
        PathBuf::from("/b.mp3"),
        PathBuf::from("/b-renamed.mp3"),
        true,
        None,
    );

    // The renamed track has no lofty Title tag, so the EXTINF label
    // queue is updated to the new file name on every playlist that
    // referenced it. The update is dispatched as a worker effect.
    assert_eq!(effects.len(), 1);
    assert!(matches!(&effects[0], Effect::UpdateExtinfTitle { .. }));
    let locations: Vec<String> = app
        .state()
        .playlist
        .tracks()
        .iter()
        .map(|track| track.display_location().into_owned())
        .collect();
    assert_eq!(
        locations,
        vec![
            "/a.mp3".to_string(),
            "/b-renamed.mp3".to_string(),
            "/c.mp3".to_string(),
            "/b-renamed.mp3".to_string(),
        ]
    );
    assert_eq!(app.state().playlist.cursor(), 1, "selection kept");
    assert_eq!(
        app.state().playback.track_index,
        Some(1),
        "playback index preserved"
    );
    assert_eq!(
        app.state().persistence.last_track,
        Some(TrackLocation::local("/b-renamed.mp3")),
        "local playback identity follows the rename"
    );
}

#[test]
fn rename_completed_updates_identity_restored_from_legacy_state() {
    let mut app = App::new();
    app.state
        .playlist
        .extend([crate::track::Track::local("/old.mp3")]);
    let persisted = PersistedState {
        last_track_path: Some("/old.mp3".to_string()),
        ..PersistedState::default()
    };

    app.apply_startup_preferences(&AppConfig::default(), &persisted);
    app.apply_rename_completed(
        PathBuf::from("/old.mp3"),
        PathBuf::from("/new.mp3"),
        true,
        None,
    );

    assert_eq!(
        app.state().persistence.last_track,
        Some(TrackLocation::local("/new.mp3"))
    );
}

#[test]
fn rename_completed_matches_normalized_source_identity() {
    let mut app = App::new();
    app.state
        .playlist
        .extend([crate::track::Track::local("/music/old.mp3")]);

    app.apply_rename_completed(
        PathBuf::from("/music/./old.mp3"),
        PathBuf::from("/music/new.mp3"),
        true,
        None,
    );

    assert_eq!(
        app.state().playlist.tracks()[0].path(),
        Some(Path::new("/music/new.mp3"))
    );
}

#[test]
fn rename_completed_with_conflict_raises_the_no_overwrite_alert() {
    let mut app = playing_fixture(1);

    let effects = app.apply_rename_completed(
        PathBuf::from("/b.mp3"),
        PathBuf::from("/solo.mp3"),
        false,
        Some(PathBuf::from("/solo.mp3")),
    );

    assert!(effects.is_empty());
    assert!(
        matches!(
            app.state().popup_dialog.active_popup_ref(),
            Some(Popup::RenameCollision { .. })
        ),
        "the conflict must surface as the no-overwrite alert"
    );
    assert_eq!(
        app.state().playlist.tracks()[1].path(),
        Some(Path::new("/b.mp3")),
        "a failed rename leaves the queue untouched"
    );
}

#[test]
fn rename_worker_owns_regular_file_collision_validation() {
    let root = unique_temp_dir("rename-worker-collision");
    let source = root.join("song.wav");
    let target = root.join("solo.wav");
    fs::write(&source, b"source").expect("source fixture");
    fs::write(&target, b"target").expect("target fixture");

    let store = PlaylistStore::for_dir(root.join("playlists"));
    let mut services = AppServices::new().expect("services construction");
    services.set_playlist_store(Some(store));
    execute_effects(
        vec![Effect::RenameFileOnDisk {
            request_id: 1,
            from: source.clone(),
            new_name: "solo.wav".to_string(),
            browser_dir: root.to_path_buf(),
        }],
        &services,
        &pending_counter(),
    );

    let event = services
        .events()
        .recv_timeout(EVENT_WAIT)
        .expect("rename collision result");
    let AppEvent::RenameCompleted {
        operation_id,
        request_id,
        from,
        to,
        result,
    } = event
    else {
        panic!("expected RenameCompleted, got {event:?}");
    };
    assert!(operation_id.get() > 0);
    assert_eq!(request_id, 1);
    assert_eq!(from, source);
    assert_eq!(to, target);
    assert_eq!(result, RenameFileResult::Conflict);
    assert_eq!(fs::read(&source).expect("source remains"), b"source");
    assert_eq!(fs::read(&target).expect("target remains"), b"target");
    services.shutdown();
}

#[test]
fn rename_worker_preserves_source_stat_path_context() {
    let root = unique_temp_dir("rename-worker-source-stat");
    let source = root.join("missing.wav");
    let target = root.join("renamed.wav");
    let store = PlaylistStore::for_dir(root.join("playlists"));
    let mut services = AppServices::new().expect("services construction");
    services.set_playlist_store(Some(store));
    execute_effects(
        vec![Effect::RenameFileOnDisk {
            request_id: 1,
            from: source.clone(),
            new_name: "renamed.wav".to_string(),
            browser_dir: root.to_path_buf(),
        }],
        &services,
        &pending_counter(),
    );

    let event = services
        .events()
        .recv_timeout(EVENT_WAIT)
        .expect("rename source-stat result");
    let AppEvent::RenameCompleted { result, .. } = event else {
        panic!("expected RenameCompleted, got {event:?}");
    };
    let RenameFileResult::Failed(error) = result else {
        panic!("expected a typed rename failure");
    };
    let source_error = error
        .source_error()
        .downcast_ref::<crate::error::HarmoniumError>()
        .expect("rename failure retains its domain cause");
    let crate::error::HarmoniumError::Io { path, source: _ } = source_error else {
        panic!("expected a path-bearing IO cause");
    };
    assert_eq!(path, &source);
    assert!(!target.exists());
    services.shutdown();
}

#[cfg(unix)]
#[test]
fn rename_worker_preserves_final_rename_path_context() {
    use std::os::unix::fs::PermissionsExt;

    let root = unique_temp_dir("rename-worker-final-rename");
    let music = root.join("music");
    fs::create_dir(&music).expect("music directory");
    let source = music.join("song.wav");
    let target = music.join("renamed.wav");
    fs::write(&source, b"source").expect("source fixture");
    fs::set_permissions(&music, fs::Permissions::from_mode(0o555)).expect("lock directory");

    let store = PlaylistStore::for_dir(root.join("playlists"));
    let mut services = AppServices::new().expect("services construction");
    services.set_playlist_store(Some(store));
    execute_effects(
        vec![Effect::RenameFileOnDisk {
            request_id: 1,
            from: source.clone(),
            new_name: "renamed.wav".to_string(),
            browser_dir: music.clone(),
        }],
        &services,
        &pending_counter(),
    );

    let event = services
        .events()
        .recv_timeout(EVENT_WAIT)
        .expect("rename final failure result");
    let AppEvent::RenameCompleted { result, .. } = event else {
        panic!("expected RenameCompleted, got {event:?}");
    };
    let RenameFileResult::Failed(error) = result else {
        panic!("expected a typed rename failure");
    };
    let source_error = error
        .source_error()
        .downcast_ref::<crate::error::HarmoniumError>()
        .expect("rename failure retains its domain cause");
    let crate::error::HarmoniumError::Io { path, source: _ } = source_error else {
        panic!("expected a path-bearing IO cause");
    };
    assert_eq!(path, &target);
    assert!(source.exists());
    assert!(!target.exists());
    fs::set_permissions(&music, fs::Permissions::from_mode(0o755)).expect("unlock directory");
    services.shutdown();
}

#[cfg(unix)]
#[test]
fn rename_rollback_failure_retains_primary_and_rollback_causes() {
    use std::os::unix::fs::PermissionsExt;

    let root = unique_temp_dir("rename-worker-rollback");
    let playlists = root.join("playlists");
    fs::create_dir(&playlists).expect("playlists directory");
    let playlist_path = playlists.join("mix.m3u8");
    fs::write(
        &playlist_path,
        format!("#EXTM3U\n{}\n", root.join("old.wav").display()),
    )
    .expect("playlist fixture");
    let store = PlaylistStore::for_dir(&playlists);
    let outcome = store
        .rewrite_path_in_all(&root.join("old.wav"), &root.join("new.wav"))
        .expect("playlist rewrite");
    assert_eq!(outcome.touched(), 1);
    fs::set_permissions(&playlists, fs::Permissions::from_mode(0o555)).expect("lock directory");

    let result = crate::filesystem::FilesystemRenameService::default().rollback_rename_rewrite(
        &store,
        &outcome,
        RenameFileResult::Failed(WorkerError::new(
            "file-rename",
            crate::error::HarmoniumError::io(
                root.join("new.wav"),
                std::io::Error::other("rename failed"),
            ),
        )),
    );

    let RenameFileResult::Failed(error) = result else {
        panic!("expected an aggregate rollback failure");
    };
    let aggregate = error
        .source_error()
        .downcast_ref::<crate::error::RollbackError>()
        .expect("rollback failure retains its aggregate source");
    assert!(aggregate.primary().downcast_ref::<WorkerError>().is_some());
    assert!(aggregate.rollback().downcast_ref::<WorkerError>().is_some());
    assert!(aggregate.to_string().contains("rename failed"));
    assert!(aggregate.to_string().contains("rollback failed"));
    fs::set_permissions(&playlists, fs::Permissions::from_mode(0o755)).expect("unlock directory");
}

#[cfg(unix)]
#[test]
fn rename_worker_rejects_symlink_targets_before_playlist_rewrite() {
    use std::os::unix::fs::symlink;

    let root = unique_temp_dir("rename-worker-symlink");
    let source = root.join("song.wav");
    let target = root.join("link.wav");
    let elsewhere = root.join("elsewhere.wav");
    fs::write(&source, b"source").expect("source fixture");
    fs::write(&elsewhere, b"elsewhere").expect("elsewhere fixture");
    symlink(&elsewhere, &target).expect("symlink fixture");

    let store = PlaylistStore::for_dir(root.join("playlists"));
    let mut services = AppServices::new().expect("services construction");
    services.set_playlist_store(Some(store));
    execute_effects(
        vec![Effect::RenameFileOnDisk {
            request_id: 1,
            from: source.clone(),
            new_name: "link.wav".to_string(),
            browser_dir: root.to_path_buf(),
        }],
        &services,
        &pending_counter(),
    );

    let event = services
        .events()
        .recv_timeout(EVENT_WAIT)
        .expect("rename symlink result");
    let AppEvent::RenameCompleted { result, .. } = event else {
        panic!("expected RenameCompleted, got {event:?}");
    };
    assert!(
        matches!(result, RenameFileResult::Rejected(message) if message.to_string().contains("symlink"))
    );
    assert!(source.exists());
    assert!(target.is_symlink());
    services.shutdown();
}

#[test]
fn rename_worker_rechecks_a_disappeared_target_and_can_complete_safely() {
    let root = unique_temp_dir("rename-worker-disappearing-target");
    let source = root.join("song.wav");
    let target = root.join("hit.wav");
    fs::write(&source, b"source").expect("source fixture");
    fs::write(&target, b"temporary target").expect("target fixture");
    fs::remove_file(&target).expect("target disappeared");

    let store = PlaylistStore::for_dir(root.join("playlists"));
    let mut services = AppServices::new().expect("services construction");
    services.set_playlist_store(Some(store));
    execute_effects(
        vec![Effect::RenameFileOnDisk {
            request_id: 1,
            from: source.clone(),
            new_name: "hit.wav".to_string(),
            browser_dir: root.to_path_buf(),
        }],
        &services,
        &pending_counter(),
    );

    let event = services
        .events()
        .recv_timeout(EVENT_WAIT)
        .expect("rename disappearing-target result");
    let AppEvent::RenameCompleted { result, .. } = event else {
        panic!("expected RenameCompleted, got {event:?}");
    };
    assert_eq!(
        result,
        RenameFileResult::Success {
            refresh_browser: true
        }
    );
    assert!(!source.exists());
    assert!(target.exists());
    services.shutdown();
}

#[test]
fn stale_file_rename_completion_cannot_mutate_state() {
    let mut app = App::new();
    let request_id = app.state_mut().async_ops.file_rename_request.begin();
    let source = PathBuf::from("/music/song.wav");
    let target = PathBuf::from("/music/hit.wav");

    let effects = app.apply_rename_request_completed(
        request_id + 1,
        source.clone(),
        target,
        RenameFileResult::Success {
            refresh_browser: false,
        },
    );

    assert!(effects.is_empty());
    assert_eq!(
        app.state().async_ops.file_rename_request.active(),
        Some(request_id)
    );
    assert!(app.state().playlist.tracks().is_empty());
}

#[test]
fn worker_collision_completion_restores_the_rename_dialog() {
    let mut app = App::new();
    let source = PathBuf::from("/music/song.wav");
    let target = PathBuf::from("/music/solo.wav");
    let request_id = app.state_mut().async_ops.file_rename_request.begin();

    let effects = app.apply_rename_request_completed(
        request_id,
        source.clone(),
        target.clone(),
        RenameFileResult::Conflict,
    );

    assert!(effects.is_empty());
    assert!(matches!(
        app.state().popup_dialog.active_popup_ref(),
        Some(Popup::RenameCollision { existing, attempted })
            if existing == &target && attempted == "solo.wav"
    ));
    assert!(matches!(
        app.state().popup_dialog.dialog_mode_value(),
        Some(DialogMode::RenameFile { path, .. }) if path == source
    ));
    assert_eq!(app.state().popup_dialog.dialog_input_value(), "solo");
    assert_eq!(
        app.state().popup_dialog.dialog_extension_value(),
        Some(".wav")
    );
}

#[test]
fn worker_safety_error_restores_the_dialog_error_state() {
    let mut app = App::new();
    let source = PathBuf::from("/music/song.wav");
    let target = PathBuf::from("/music/link.wav");
    let request_id = app.state_mut().async_ops.file_rename_request.begin();

    app.apply_rename_request_completed(
        request_id,
        source,
        target,
        RenameFileResult::Rejected(WorkerError::message(
            "file-rename",
            "refusing to rename onto a symlink",
        )),
    );

    assert!(matches!(
        app.state().popup_dialog.dialog_mode_value(),
        Some(DialogMode::RenameFile { error: Some(message), .. })
            if message.contains("symlink")
    ));
    assert!(app.state().popup_dialog.active_popup_ref().is_none());
}

#[test]
fn rename_from_browser_propagates_to_playlists_and_keeps_playback() {
    let root = unique_temp_dir("e2e-rename-browser");
    let music = root.join("music");
    fs::create_dir_all(&music).expect("music dir");
    let song = music.join("song.wav");
    let marked = music.join("marked.wav");
    fs::write(&song, crate::test_support::wav_bytes(&[])).expect("song fixture");
    fs::write(&marked, crate::test_support::wav_bytes(&[])).expect("marked fixture");
    let playlists = root.join("playlists");
    fs::create_dir_all(&playlists).expect("playlists dir");
    let saved = format!(
        "#EXTM3U\n#PLAYLIST:Evening mix\n#EXTINF:253,Rush - Tom Sawyer\n{}\n",
        song.display()
    );
    fs::write(playlists.join("evening.m3u8"), &saved).expect("playlist fixture");

    let store = PlaylistStore::for_dir(&playlists);
    let mut app = App::from_config_and_store(KeysConfig::default(), store.clone());
    let mut services = AppServices::new().expect("services construction");
    services.set_playlist_store(Some(store));

    // Queue both files, mark the middle one as playing at index 0
    app.state_mut()
        .extend_playlist([song.clone(), marked.clone()]);
    app.state_mut().playlist.select(0);
    app.state_mut().playback.track_index = Some(0);

    // Browser on the music dir; mark one entry but rename only the cursor
    app.state_mut().change_browser_dir(
        music.clone(),
        crate::filesystem::read_sorted_entries(&music, false).expect("listing"),
    );
    app.state_mut()
        .browser
        .selected_entries
        .insert(marked.clone());
    let song_index = app
        .state()
        .browser
        .entries
        .iter()
        .position(|entry| entry.name == "song.wav")
        .expect("song entry");
    app.state_mut().browser.set_cursor(song_index);

    app.handle_command(Command::RenameFile);
    assert_eq!(
        app.state().popup_dialog.dialog_input_value(),
        "song",
        "preloaded base name"
    );
    assert_eq!(
        app.state().popup_dialog.dialog_extension_value(),
        Some(".wav"),
        "the extension is locked"
    );
    // Only the base is editable; the rename appends the locked extension on commit.
    app.state_mut()
        .popup_dialog
        .set_dialog_input("hit".to_string());
    let effects = app.handle_dialog_key_event(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert_eq!(
        effects,
        vec![Effect::RenameFileOnDisk {
            request_id: 1,
            from: song.clone(),
            new_name: "hit.wav".to_string(),
            browser_dir: music.clone(),
        }]
    );
    execute_effects(effects, &services, &pending_counter());

    let (request_id, from, to, result) = match services.events().recv_timeout(EVENT_WAIT) {
        Ok(AppEvent::RenameCompleted {
            request_id,
            from,
            to,
            result,
            ..
        }) => (request_id, from, to, result),
        Ok(event) => panic!("expected RenameCompleted, got {event:?}"),
        Err(error) => panic!("bus receive failed: {error}"),
    };
    assert!(matches!(&result, RenameFileResult::Success { .. }));
    let follow_ups = app.apply_rename_request_completed(request_id, from, to, result);
    execute_effects(follow_ups, &services, &pending_counter());

    // Disk: only the cursor file moved, the marked entry is untouched
    assert!(!song.exists(), "the old name must be gone");
    assert!(music.join("hit.wav").exists(), "the new file must exist");
    assert!(marked.exists(), "marked entries are never renamed");

    // Playlist: the saved m3u8 references the new path, EXTINF preserved
    let raw = fs::read_to_string(playlists.join("evening.m3u8")).expect("read playlist");
    assert!(
        raw.contains(&music.join("hit.wav").display().to_string()),
        "the playlist must reference the new path, got: {raw}"
    );
    assert!(
        raw.contains("#EXTINF:253,Rush - Tom Sawyer"),
        "EXTINF must survive the rewrite, got: {raw}"
    );

    // Queue: index and cursor survive, the path is the renamed file
    assert_eq!(
        app.state().playlist.tracks()[0].path(),
        Some(music.join("hit.wav").as_path())
    );
    assert_eq!(app.state().playback.track_index, Some(0));

    services.shutdown();
}

#[test]
fn rename_from_playlist_rewrites_the_queue_and_the_document() {
    let root = unique_temp_dir("e2e-rename-playlist");
    let music = root.join("music");
    fs::create_dir_all(&music).expect("music dir");
    let song = music.join("song.wav");
    fs::write(&song, crate::test_support::wav_bytes(&[])).expect("song fixture");
    let playlists = root.join("playlists");
    fs::create_dir_all(&playlists).expect("playlists dir");
    fs::write(
        playlists.join("mix.m3u8"),
        format!("#EXTM3U\n#EXTINF:-1,Portable\n{}\n", song.display()),
    )
    .expect("playlist fixture");

    let store = PlaylistStore::for_dir(&playlists);
    let mut app = App::from_config_and_store(KeysConfig::default(), store.clone());
    let mut services = AppServices::new().expect("services construction");
    services.set_playlist_store(Some(store));

    app.state_mut().extend_playlist([song.clone()]);
    app.state_mut().playlist.select(0);
    app.state_mut().active_panel = Panel::Playlist;

    app.handle_command(Command::RenameFile);
    assert_eq!(
        app.state().popup_dialog.dialog_input_value(),
        "song",
        "base name only"
    );
    assert_eq!(
        app.state().popup_dialog.dialog_extension_value(),
        Some(".wav"),
        "extension stays locked"
    );
    // Editable portion excludes the extension; the commit appends it.
    app.state_mut()
        .popup_dialog
        .set_dialog_input("renamed".to_string());
    let effects = app.handle_dialog_key_event(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert_eq!(effects.len(), 1, "the rename effect is dispatched");
    execute_effects(effects, &services, &pending_counter());

    let (request_id, from, to, result) = match services.events().recv_timeout(EVENT_WAIT) {
        Ok(AppEvent::RenameCompleted {
            request_id,
            from,
            to,
            result,
            ..
        }) => (request_id, from, to, result),
        Ok(event) => panic!("expected RenameCompleted, got {event:?}"),
        Err(error) => panic!("bus receive failed: {error}"),
    };
    assert!(matches!(&result, RenameFileResult::Success { .. }));
    let follow_ups = app.apply_rename_request_completed(request_id, from, to, result);
    execute_effects(follow_ups, &services, &pending_counter());

    assert!(music.join("renamed.wav").exists());
    assert!(!song.exists());
    assert_eq!(
        app.state().playlist.tracks()[0].path(),
        Some(music.join("renamed.wav").as_path()),
        "the queued entry follows the rename"
    );
    assert_eq!(app.state().playlist.cursor(), 0);
    let raw = fs::read_to_string(playlists.join("mix.m3u8")).expect("read playlist");
    assert!(
        raw.contains(&music.join("renamed.wav").display().to_string()),
        "the saved playlist must reference the renamed file, got: {raw}"
    );

    services.shutdown();
}

/// When the renamed track has no lofty Title tag, the rename completion
/// dispatches an `Effect::UpdateExtinfTitle` so every saved playlist
/// picks up the new file name on its EXTINF line. When the track
/// already has a Title tag, no update is queued: the EXTINF keeps the
/// lofty title the user curated.
#[test]
fn rename_without_title_updates_extinf_across_saved_playlists() {
    let root = unique_temp_dir("e2e-rename-extinf");
    let music = root.join("music");
    fs::create_dir_all(&music).expect("music dir");
    let song = music.join("song.wav");
    // No INAM tag: the track has no lofty Title.
    fs::write(&song, crate::test_support::wav_bytes(&[])).expect("song fixture");
    let playlists = root.join("playlists");
    fs::create_dir_all(&playlists).expect("playlists dir");
    fs::write(
        playlists.join("evening.m3u8"),
        format!("#EXTM3U\n#EXTINF:253,song.wav\n{}\n", song.display()),
    )
    .expect("playlist fixture");

    let store = PlaylistStore::for_dir(&playlists);
    let mut app = App::from_config_and_store(KeysConfig::default(), store.clone());
    let mut services = AppServices::new().expect("services construction");
    services.set_playlist_store(Some(store));

    let effects = app.apply_rename_completed(song.clone(), music.join("hit.wav"), true, None);

    // The renamed track has no lofty Title, so the EXTINF queue must
    // pick up the new file name on the playlist.
    let extinf_effect = effects
        .iter()
        .find_map(|effect| match effect {
            Effect::UpdateExtinfTitle { new_title, .. } => Some(new_title.as_str()),
            _ => None,
        })
        .expect("rename of a tagless track queues an EXTINF title update");
    assert_eq!(extinf_effect, "hit.wav");

    services.shutdown();
}

#[test]
fn rename_with_lofty_title_does_not_queue_extinf_update() {
    let root = unique_temp_dir("e2e-rename-extinf-keep");
    let music = root.join("music");
    fs::create_dir_all(&music).expect("music dir");
    let song = music.join("song.wav");
    // Carry a Title tag so the rename flow treats the EXTINF as curated.
    fs::write(
        &song,
        crate::test_support::wav_bytes(&[("INAM", "Tom Sawyer")]),
    )
    .expect("song fixture");

    let store = PlaylistStore::for_dir(root.join("playlists"));
    let mut app = App::from_config_and_store(KeysConfig::default(), store.clone());

    // Simulate a metadata worker pass that delivered the lofty snapshot
    // to the queue. Without this attach, the in-memory metadata would
    // stay `None` and the rename would fall back to the "no Title" path.
    app.state_mut().extend_playlist([song.clone()]);
    let meta = TrackMetadata {
        title: "Tom Sawyer".into(),
        title_tagged: true,
        artist: "Rush".into(),
        album: String::new(),
        track_number: None,
        duration: std::time::Duration::from_secs(253),
        bitrate: None,
        sample_rate: None,
        codec: "WAV".into(),
        format: "WAV".into(),
        album_artist: None,
        disc_number: None,
        genre: None,
        year: None,
        composer: None,
        comment: None,
    };
    app.state_mut().playlist.apply_metadata(&song, meta);

    let effects = app.apply_rename_completed(song.clone(), music.join("hit.wav"), true, None);

    assert!(
        !effects
            .iter()
            .any(|effect| matches!(effect, Effect::UpdateExtinfTitle { .. })),
        "a rename must not touch the EXTINF when the track already has a Title"
    );
}

#[cfg(unix)]
#[test]
fn rename_aborts_when_a_playlist_rewrite_fails_and_keeps_the_old_name() {
    use std::os::unix::fs::PermissionsExt;

    let root = unique_temp_dir("e2e-rename-abort");
    let music = root.join("music");
    fs::create_dir_all(&music).expect("music dir");
    let song = music.join("song.wav");
    fs::write(&song, crate::test_support::wav_bytes(&[])).expect("song fixture");
    let playlists = root.join("playlists");
    fs::create_dir_all(&playlists).expect("playlists dir");
    fs::write(
        playlists.join("evening.m3u8"),
        format!("#EXTM3U\n#EXTINF:-1,Mix\n{}\n", song.display()),
    )
    .expect("playlist fixture");

    // Make the saved playlist unwritable. The rewrite replaces documents
    // atomically (temp file + rename), so the operative permission is on
    // the directory: with the playlists dir read-only the temp write fails
    // and the whole rewrite must abort before any `fs::rename` of the
    // audio file.
    fs::set_permissions(&playlists, fs::Permissions::from_mode(0o555)).expect("lock dir");
    fs::set_permissions(
        playlists.join("evening.m3u8"),
        fs::Permissions::from_mode(0o444),
    )
    .expect("lock playlist");

    let store = PlaylistStore::for_dir(&playlists);
    let mut app = App::from_config_and_store(KeysConfig::default(), store.clone());
    let mut services = AppServices::new().expect("services construction");
    services.set_playlist_store(Some(store));

    app.state_mut().extend_playlist([song.clone()]);
    app.state_mut().playlist.select(0);
    app.state_mut().active_panel = Panel::Playlist;

    app.handle_command(Command::RenameFile);
    assert_eq!(
        app.state().popup_dialog.dialog_input_value(),
        "song",
        "base name only"
    );
    app.state_mut()
        .popup_dialog
        .set_dialog_input("hit".to_string());
    let effects = app.handle_dialog_key_event(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert_eq!(
        effects,
        vec![Effect::RenameFileOnDisk {
            request_id: 1,
            from: song.clone(),
            new_name: "hit.wav".to_string(),
            browser_dir: PathBuf::from("."),
        }]
    );
    execute_effects(effects, &services, &pending_counter());

    let (request_id, from, to, result) = match services.events().recv_timeout(EVENT_WAIT) {
        Ok(AppEvent::RenameCompleted {
            request_id,
            from,
            to,
            result,
            ..
        }) => (request_id, from, to, result),
        Ok(event) => panic!("expected RenameCompleted, got {event:?}"),
        Err(error) => panic!("bus receive failed: {error}"),
    };
    assert!(matches!(&result, RenameFileResult::Failed(_)));
    let follow_ups = app.apply_rename_request_completed(request_id, from, to, result);
    execute_effects(follow_ups, &services, &pending_counter());

    // The audio file keeps its old name: the disk rename was never reached
    assert!(song.exists(), "the file must keep its old name");
    assert!(!music.join("hit.wav").exists(), "no new file may appear");
    assert_eq!(
        app.state().playlist.tracks()[0].path(),
        Some(song.as_path()),
        "the queue keeps the old path"
    );
    assert!(
        app.state()
            .notifications
            .iter()
            .any(|message| message.contains("Rename failed")),
        "the failure must be visible to the user"
    );
    let raw = fs::read_to_string(playlists.join("evening.m3u8")).expect("read playlist");
    assert!(
        raw.contains(&song.display().to_string()),
        "the playlist must keep the old reference, got: {raw}"
    );

    // Restore permissions so the temp-dir cleanup can remove the files
    fs::set_permissions(&playlists, fs::Permissions::from_mode(0o755)).expect("unlock dir");

    services.shutdown();
}

#[test]
fn metadata_edit_from_browser_round_trips_into_the_queue() {
    let root = unique_temp_dir("e2e-metadata-browser");
    let music = root.join("music");
    fs::create_dir_all(&music).expect("music dir");
    let song = music.join("song.wav");
    fs::write(
        &song,
        crate::test_support::wav_bytes(&[("INAM", "Old Title")]),
    )
    .expect("song fixture");

    let store = PlaylistStore::for_dir(root.join("playlists"));
    let mut app = App::from_config_and_store(KeysConfig::default(), store.clone());
    let mut services = AppServices::new().expect("services construction");
    services.set_playlist_store(Some(store));

    app.state_mut().extend_playlist([song.clone()]);
    app.state_mut().change_browser_dir(
        music.clone(),
        crate::filesystem::read_sorted_entries(&music, false).expect("listing"),
    );

    // Open the editor: the prefill effect must arrive on the bus
    let effects = app.handle_command(Command::EditMetadata);
    assert_eq!(effects.len(), 1, "the prefill effect is dispatched");
    execute_effects(effects, &services, &pending_counter());
    let (prefill_path, fields) = match services.events().recv_timeout(EVENT_WAIT) {
        Ok(AppEvent::MetadataPrefillReady { path, fields, .. }) => (path, fields),
        Ok(event) => panic!("expected MetadataPrefillReady, got {event:?}"),
        Err(error) => panic!("bus receive failed: {error}"),
    };
    app.apply_metadata_prefill_ready(prefill_path, fields);
    match app.state().popup_dialog.dialog_mode_ref() {
        Some(DialogMode::EditMetadata {
            fields, loading, ..
        }) => {
            assert!(!loading, "the form leaves the loading state");
            assert_eq!(fields[0], "Old Title", "the tag prefills the form");
        }
        other => panic!("expected EditMetadata dialog, got {other:?}"),
    }

    // Edit the title, walk to the last field with the arrow keys, add a comment
    // and confirm with Enter (which now saves every field at once).
    let enter = || KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
    let down = || KeyEvent::new(KeyCode::Down, KeyModifiers::NONE);
    app.state_mut()
        .popup_dialog
        .set_dialog_input("New Title".to_string());
    for _ in 0..9 {
        app.handle_dialog_key_event(down());
    }
    app.state_mut()
        .popup_dialog
        .set_dialog_input("Great song".to_string());
    let effects = app.handle_dialog_key_event(enter());
    assert_eq!(effects.len(), 1, "Enter dispatches the write");
    execute_effects(effects, &services, &pending_counter());

    match services.events().recv_timeout(EVENT_WAIT) {
        Ok(AppEvent::MetadataWriteCompleted {
            path,
            result: Ok(()),
            ..
        }) => assert_eq!(path, song),
        Ok(event) => panic!("expected MetadataWriteCompleted, got {event:?}"),
        Err(error) => panic!("bus receive failed: {error}"),
    }
    let effects = app.apply_metadata_write_completed(
        song.clone(),
        Ok(()),
        [
            "Great song".to_string(),
            String::new(),
            String::new(),
            String::new(),
            String::new(),
            String::new(),
            String::new(),
            String::new(),
            String::new(),
            String::new(),
        ],
    );
    execute_effects(effects, &services, &pending_counter());
    match services.events().recv_timeout(EVENT_WAIT) {
        Ok(AppEvent::MetadataCompleted { loaded, failed, .. }) => {
            assert_eq!(failed, 0);
            assert_eq!(loaded.len(), 1, "the re-extraction refreshes the track");
            // Commit the fresh snapshot so the queue shows the new title
            app.apply_metadata_completed(loaded, failed);
        }
        Ok(event) => panic!("expected MetadataCompleted, got {event:?}"),
        Err(error) => panic!("bus receive failed: {error}"),
    }

    // Disk and queue agree on the new values
    let meta = crate::metadata::reader::read_metadata(&song).expect("re-read");
    assert_eq!(meta.title, "New Title");
    assert_eq!(meta.comment.as_deref(), Some("Great song"));
    assert_eq!(
        app.state().playlist.tracks()[0]
            .metadata()
            .map(|m| m.title.as_str()),
        Some("New Title"),
        "the queued track shows the new title"
    );

    services.shutdown();
}

#[test]
fn metadata_edit_from_playlist_writes_the_file_tags() {
    let root = unique_temp_dir("e2e-metadata-playlist");
    let music = root.join("music");
    fs::create_dir_all(&music).expect("music dir");
    let song = music.join("song.wav");
    fs::write(&song, crate::test_support::wav_bytes(&[])).expect("song fixture");

    let store = PlaylistStore::for_dir(root.join("playlists"));
    let mut app = App::from_config_and_store(KeysConfig::default(), store.clone());
    let mut services = AppServices::new().expect("services construction");
    services.set_playlist_store(Some(store));

    app.state_mut().extend_playlist([song.clone()]);
    app.state_mut().playlist.select(0);
    app.state_mut().active_panel = Panel::Playlist;

    let effects = app.handle_command(Command::EditMetadata);
    execute_effects(effects, &services, &pending_counter());
    let (prefill_path, fields) = match services.events().recv_timeout(EVENT_WAIT) {
        Ok(AppEvent::MetadataPrefillReady { path, fields, .. }) => (path, fields),
        Ok(event) => panic!("expected MetadataPrefillReady, got {event:?}"),
        Err(error) => panic!("bus receive failed: {error}"),
    };
    app.apply_metadata_prefill_ready(prefill_path, fields);

    // Tagless file: edit the Title field, walk to the end with the arrow keys
    // and confirm with Enter, which now saves every field at once.
    let enter = || KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
    let down = || KeyEvent::new(KeyCode::Down, KeyModifiers::NONE);
    app.state_mut()
        .popup_dialog
        .set_dialog_input("Rush".to_string());
    for _ in 0..9 {
        app.handle_dialog_key_event(down());
    }
    let effects = app.handle_dialog_key_event(enter());
    execute_effects(effects, &services, &pending_counter());
    match services.events().recv_timeout(EVENT_WAIT) {
        Ok(AppEvent::MetadataWriteCompleted { result: Ok(()), .. }) => {}
        Ok(event) => panic!("expected MetadataWriteCompleted, got {event:?}"),
        Err(error) => panic!("bus receive failed: {error}"),
    }

    let meta = crate::metadata::reader::read_metadata(&song).expect("re-read");
    assert_eq!(meta.title, "Rush", "the edited title lands on disk");

    services.shutdown();
}

/// When the metadata editor writes a new Title, every saved playlist
/// that references the file must pick up the new title on its EXTINF
/// line. Clearing the Title leaves the playlists untouched (the next
/// render pass will use the file name as the fallback).
#[test]
fn metadata_write_propagates_the_new_title_into_extinf_labels() {
    let root = unique_temp_dir("e2e-metadata-write-extinf");
    let music = root.join("music");
    fs::create_dir_all(&music).expect("music dir");
    let song = music.join("song.wav");
    fs::write(&song, crate::test_support::wav_bytes(&[])).expect("song fixture");
    let playlists = root.join("playlists");
    fs::create_dir_all(&playlists).expect("playlists dir");
    fs::write(
        playlists.join("evening.m3u8"),
        format!("#EXTM3U\n#EXTINF:253,song.wav\n{}\n", song.display()),
    )
    .expect("playlist fixture");

    let store = PlaylistStore::for_dir(&playlists);
    let mut app = App::from_config_and_store(KeysConfig::default(), store.clone());

    // Simulate the worker reporting a successful Title write.
    let fields = std::array::from_fn(|index| {
        if index == 0 {
            "Tom Sawyer".to_string()
        } else {
            String::new()
        }
    });
    let effects = app.apply_metadata_write_completed(song.clone(), Ok(()), fields);

    let extinf_effect = effects
        .iter()
        .find_map(|effect| match effect {
            Effect::UpdateExtinfTitle { new_title, .. } => Some(new_title.clone()),
            _ => None,
        })
        .expect("a successful Title write queues an EXTINF title update");
    assert_eq!(extinf_effect, "Tom Sawyer");
}

/// When the metadata editor clears the Title (writes an empty string),
/// the EXTINF label of saved playlists must stay untouched: the
/// existing label describes the file as well as before, and the next
/// render pass will replace it with the file name on its own.
#[test]
fn metadata_write_that_clears_title_does_not_queue_extinf_update() {
    let root = unique_temp_dir("e2e-metadata-write-extinf-clear");
    let music = root.join("music");
    fs::create_dir_all(&music).expect("music dir");
    let song = music.join("song.wav");
    fs::write(&song, crate::test_support::wav_bytes(&[])).expect("song fixture");

    let store = PlaylistStore::for_dir(root.join("playlists"));
    let mut app = App::from_config_and_store(KeysConfig::default(), store.clone());

    let fields = std::array::from_fn(|_| String::new());
    let effects = app.apply_metadata_write_completed(song.clone(), Ok(()), fields);

    assert!(
        !effects
            .iter()
            .any(|effect| matches!(effect, Effect::UpdateExtinfTitle { .. })),
        "clearing the Title must not touch the EXTINF label"
    );
}

#[test]
fn metadata_save_on_a_read_only_file_reports_an_error_and_keeps_the_old_tags() {
    let root = unique_temp_dir("e2e-metadata-write-fail");
    let music = root.join("music");
    fs::create_dir_all(&music).expect("music dir");
    let song = music.join("song.wav");
    fs::write(
        &song,
        crate::test_support::wav_bytes(&[("INAM", "Old Title")]),
    )
    .expect("song fixture");

    // Lock the file: the write must fail while reading the tags still works
    let mut permissions = fs::metadata(&song).expect("metadata").permissions();
    permissions.set_readonly(true);
    fs::set_permissions(&song, permissions).expect("lock fixture");

    let store = PlaylistStore::for_dir(root.join("playlists"));
    let mut app = App::from_config_and_store(KeysConfig::default(), store.clone());
    let mut services = AppServices::new().expect("services construction");
    services.set_playlist_store(Some(store));

    app.state_mut().extend_playlist([song.clone()]);
    app.state_mut().change_browser_dir(
        music.clone(),
        crate::filesystem::read_sorted_entries(&music, false).expect("listing"),
    );

    // Open the editor: the prefill reads the tags (read-only still reads)
    let effects = app.handle_command(Command::EditMetadata);
    assert_eq!(effects.len(), 1, "the prefill effect is dispatched");
    execute_effects(effects, &services, &pending_counter());
    let (prefill_path, fields) = match services.events().recv_timeout(EVENT_WAIT) {
        Ok(AppEvent::MetadataPrefillReady { path, fields, .. }) => (path, fields),
        Ok(event) => panic!("expected MetadataPrefillReady, got {event:?}"),
        Err(error) => panic!("bus receive failed: {error}"),
    };
    app.apply_metadata_prefill_ready(prefill_path, fields);
    match app.state().popup_dialog.dialog_mode_ref() {
        Some(DialogMode::EditMetadata {
            fields, loading, ..
        }) => {
            assert!(!loading, "the form leaves the loading state");
            assert_eq!(fields[0], "Old Title", "the tag prefills the form");
        }
        other => panic!("expected EditMetadata dialog, got {other:?}"),
    }

    // Edit the title, walk to the last field with the arrow keys and confirm
    // with Enter, which now saves every field at once.
    let enter = || KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
    let down = || KeyEvent::new(KeyCode::Down, KeyModifiers::NONE);
    app.state_mut()
        .popup_dialog
        .set_dialog_input("New Title".to_string());
    for _ in 0..9 {
        app.handle_dialog_key_event(down());
    }
    let effects = app.handle_dialog_key_event(enter());
    assert_eq!(effects.len(), 1, "Enter dispatches the write");
    execute_effects(effects, &services, &pending_counter());

    let (path, result, fields) = match services.events().recv_timeout(EVENT_WAIT) {
        Ok(AppEvent::MetadataWriteCompleted {
            path,
            result,
            fields,
            ..
        }) => (path, result, fields),
        Ok(event) => panic!("expected MetadataWriteCompleted, got {event:?}"),
        Err(error) => panic!("bus receive failed: {error}"),
    };
    assert!(result.is_err(), "a read-only file must fail the save");
    let follow_ups = app.apply_metadata_write_completed(path, result, fields);
    assert!(
        follow_ups.is_empty(),
        "a failed save never re-extracts the file"
    );
    execute_effects(follow_ups, &services, &pending_counter());

    // The file keeps its old tags and the in-memory track is untouched
    let meta = crate::metadata::reader::read_metadata(&song).expect("re-read");
    assert_eq!(meta.title, "Old Title", "the file tags are unchanged");
    assert_eq!(meta.comment, None);
    assert!(
        app.state().playlist.tracks()[0].metadata().is_none(),
        "the in-memory track keeps its old metadata"
    );
    assert!(
        app.state()
            .notifications
            .iter()
            .any(|message| message.contains("Could not save tags")),
        "the failure must be visible to the user"
    );

    services.shutdown();
}

#[test]
fn edit_metadata_on_a_non_audio_entry_shows_a_notification_only() {
    let root = unique_temp_dir("e2e-metadata-nonaudio");
    fs::write(root.join("notes.txt"), b"text").expect("fixture");

    let mut app = App::new();
    app.state_mut().change_browser_dir(
        root.to_path_buf(),
        crate::filesystem::read_sorted_entries(&root, false).expect("listing"),
    );

    app.handle_command(Command::EditMetadata);

    assert!(
        app.state().popup_dialog.dialog_mode_ref().is_none(),
        "no form for text files"
    );
    assert!(
        app.state()
            .notifications
            .iter()
            .any(|message| message.contains("edit")),
        "the rejection must be visible"
    );
}

/// Queue /a.mp3 /b.mp3 /c.mp3 and pretend the cursor entry is playing.
fn playing_fixture(cursor: usize) -> App {
    let mut app = App::new();
    app.state
        .extend_playlist(["/a.mp3", "/b.mp3", "/c.mp3"].map(PathBuf::from));
    app.state.playlist.select(cursor);
    app.state.playback.track_index = Some(cursor);
    app.state.playback.status = PlayStatus::Playing;
    app
}

#[test]
fn crossfade_arms_the_following_track_only_when_enabled() {
    // Off (0 s): the engine is never asked to preload a next track.
    let mut app = playing_fixture(0);
    app.config.playback.crossfade_seconds = crossfade(0);
    assert!(
        !app.begin_current_track()
            .iter()
            .any(|effect| matches!(effect, Effect::Audio(AudioCommand::PreloadNext { .. }))),
        "0 s must not arm a crossfade"
    );

    // Enabled: the next sequential entry is preloaded, without mutating the
    // real navigation yet.
    app.config.playback.crossfade_seconds = crossfade(15);
    let armed = app
        .begin_current_track()
        .into_iter()
        .find(|effect| matches!(effect, Effect::Audio(AudioCommand::PreloadNext { .. })))
        .expect("enabled crossfade arms next");
    match armed {
        Effect::Audio(AudioCommand::PreloadNext {
            source,
            track_index,
        }) => {
            assert_eq!(track_index, 1, "must preload the next entry");
            assert_eq!(
                source.display_location(),
                "/b.mp3",
                "expected the second entry, got {source:?}"
            );
        }
        other => panic!("expected PreloadNext, got {other:?}"),
    }

    // The real navigation must not have advanced from the peek.
    assert_eq!(app.state.playback.track_index, Some(0));

    // A tail entry with no sequential next arms nothing.
    let mut tail = playing_fixture(2);
    tail.config.playback.crossfade_seconds = crossfade(15);
    assert!(
        !tail
            .begin_current_track()
            .iter()
            .any(|effect| matches!(effect, Effect::Audio(AudioCommand::PreloadNext { .. }))),
        "last track has no next to arm"
    );
}

#[test]
fn crossfade_completion_adopts_track_b_elapsed_and_preserves_follow_up_work() {
    let mut app = playing_fixture(0);
    app.config.playback.crossfade_seconds = crossfade(15);
    app.state.artwork.set_enabled(true);
    app.state.lyrics.visible = true;

    let mut metadata = TrackMetadata::default();
    metadata.duration = Duration::from_secs(90);
    app.state
        .playlist
        .apply_metadata(Path::new("/b.mp3"), metadata);

    let elapsed = Duration::from_millis(1_250);
    let effects = app.apply_crossfade_completed(99, PathBuf::from("/album/../b.mp3"), elapsed);

    assert_eq!(app.state.playback.track_index, Some(1));
    assert_eq!(app.state.playback.elapsed, elapsed);
    assert_eq!(app.state.persistence.last_track_position_ms, 1_250);
    assert_eq!(app.state.playback.duration, Some(Duration::from_secs(90)));
    assert_eq!(
        app.state.persistence.last_track,
        Some(TrackLocation::local("/b.mp3"))
    );
    assert!(effects.iter().any(|effect| matches!(
        effect,
        Effect::LoadArtwork {
            track_index: 1,
            path,
            ..
        } if path == &Some(PathBuf::from("/b.mp3"))
    )));
    assert!(
        effects
            .iter()
            .any(|effect| matches!(effect, Effect::LoadLyrics { track_index: 1, .. }))
    );
    assert!(effects.iter().any(|effect| matches!(
        effect,
        Effect::Audio(AudioCommand::PreloadNext {
            track_index: 2,
            source,
        }) if source.display_location() == "/c.mp3"
    )));
}

#[test]
fn stale_or_unknown_crossfade_completion_is_ignored() {
    let mut app = playing_fixture(0);
    app.state.playback.elapsed = Duration::from_secs(3);
    app.state.persistence.last_track_position_ms = 3_000;

    let effects = app.apply_crossfade_completed(
        99,
        PathBuf::from("/no-longer-queued.mp3"),
        Duration::from_secs(1),
    );

    assert!(effects.is_empty());
    assert_eq!(app.state.playback.track_index, Some(0));
    assert_eq!(app.state.playback.elapsed, Duration::from_secs(3));
    assert_eq!(app.state.persistence.last_track_position_ms, 3_000);
}

fn audio_commands(effects: &[Effect]) -> Vec<AudioCommand> {
    effects
        .iter()
        .filter_map(|effect| match effect {
            Effect::Audio(command) => Some(command.clone()),
            _ => None,
        })
        .collect()
}

#[test]
fn toggle_pause_flips_between_playing_and_paused() {
    let mut app = playing_fixture(0);

    let effects = app.handle_command(Command::TogglePause);
    assert_eq!(audio_commands(&effects), vec![AudioCommand::Pause]);
    assert_eq!(app.state().playback.status, PlayStatus::Paused);

    let effects = app.handle_command(Command::TogglePause);
    assert_eq!(audio_commands(&effects), vec![AudioCommand::Resume]);
    assert_eq!(app.state().playback.status, PlayStatus::Playing);
}

#[test]
fn toggle_pause_is_a_no_op_while_stopped() {
    let mut app = App::new();

    let effects = app.handle_command(Command::TogglePause);

    assert!(effects.is_empty());
    assert_eq!(app.state().playback.status, PlayStatus::Stopped);
}

#[test]
fn next_track_starts_the_following_entry_from_the_top() {
    let mut app = playing_fixture(0);

    let effects = app.handle_command(Command::NextTrack);

    assert_eq!(
        audio_commands(&effects),
        vec![AudioCommand::Play {
            source: crate::stream::TrackSource::local("/b.mp3"),
            track_index: 1,
            generation: None,
        }]
    );
    assert_eq!(app.state().playback.track_index, Some(1));
    assert_eq!(app.state().playback.status, PlayStatus::Playing);
    assert_eq!(app.state().playback.elapsed, Duration::ZERO);
    assert_eq!(app.state().playlist.cursor(), 1);
}

#[test]
fn next_track_at_the_tail_notifies_and_keeps_playing() {
    let mut app = playing_fixture(2);

    let effects = app.handle_command(Command::NextTrack);

    assert!(effects.is_empty());
    assert_eq!(app.state().playback.track_index, Some(2));
    assert_eq!(app.state().playback.status, PlayStatus::Playing);
    assert_eq!(
        app.state().notifications.last(),
        Some(&"No next track".to_string())
    );
}

#[test]
fn previous_track_deep_into_the_song_restarts_it() {
    let mut app = playing_fixture(1);
    app.state.playback.elapsed = Duration::from_secs(10);

    let effects = app.handle_command(Command::PreviousTrack);

    assert_eq!(
        audio_commands(&effects),
        vec![AudioCommand::SeekTo(Duration::ZERO)]
    );
    // The queue cursor must not move on a restart
    assert_eq!(app.state().playlist.cursor(), 1);
    assert_eq!(app.state().playback.track_index, Some(1));
    assert_eq!(app.state().playback.elapsed, Duration::ZERO);
}

#[test]
fn previous_track_near_the_start_switches_to_the_predecessor() {
    let mut app = playing_fixture(2);
    app.state.playback.elapsed = Duration::from_secs(1);

    let effects = app.handle_command(Command::PreviousTrack);

    assert_eq!(
        audio_commands(&effects),
        vec![AudioCommand::Play {
            source: crate::stream::TrackSource::local("/b.mp3"),
            track_index: 1,
            generation: None,
        }]
    );
    assert_eq!(app.state().playlist.cursor(), 1);
}

#[test]
fn previous_track_on_the_first_entry_restarts_it() {
    let mut app = playing_fixture(0);
    app.state.playback.elapsed = Duration::from_secs(1);

    let effects = app.handle_command(Command::PreviousTrack);

    assert_eq!(
        audio_commands(&effects),
        vec![AudioCommand::SeekTo(Duration::ZERO)]
    );
    assert_eq!(app.state().playlist.cursor(), 0);
}

#[test]
fn volume_steps_saturate_and_report_each_value() {
    let mut app = playing_fixture(0);
    app.state.playback.volume_percent = volume(98);

    let up = app.handle_command(Command::VolumeUp);
    assert_eq!(
        audio_commands(&up),
        vec![AudioCommand::SetVolume(volume(100))]
    );
    assert_eq!(app.state().playback.volume_percent, volume(100));

    let pinned = app.handle_command(Command::VolumeUp);
    assert_eq!(
        audio_commands(&pinned),
        vec![AudioCommand::SetVolume(volume(100))]
    );

    app.state.playback.volume_percent = volume(3);
    let down = app.handle_command(Command::VolumeDown);
    assert_eq!(
        audio_commands(&down),
        vec![AudioCommand::SetVolume(volume(0))]
    );
    assert_eq!(app.state().playback.volume_percent, volume(0));
}

#[test]
fn speed_steps_saturate_bounds_and_reset_returns_to_default() {
    use crate::audio::playback::{SPEED_DEFAULT, SPEED_MAX, SPEED_MIN, SPEED_STEP};
    let mut app = playing_fixture(0);
    app.state.playback.speed = SPEED_DEFAULT;

    let up = app.handle_command(Command::SpeedUp);
    assert_eq!(
        audio_commands(&up),
        vec![AudioCommand::SetSpeed(stepped_speed(
            SPEED_DEFAULT,
            SPEED_STEP
        ))]
    );
    assert_eq!(
        app.state().playback.speed,
        stepped_speed(SPEED_DEFAULT, SPEED_STEP)
    );

    // Saturate at the maximum: repeated presses never exceed it.
    app.state.playback.speed = SPEED_MAX;
    let pinned = app.handle_command(Command::SpeedUp);
    assert_eq!(
        audio_commands(&pinned),
        vec![AudioCommand::SetSpeed(SPEED_MAX)]
    );
    assert_eq!(app.state().playback.speed, SPEED_MAX);

    // Saturate at the minimum on the other side.
    app.state.playback.speed = SPEED_MIN;
    let down = app.handle_command(Command::SpeedDown);
    assert_eq!(
        audio_commands(&down),
        vec![AudioCommand::SetSpeed(SPEED_MIN)]
    );
    assert_eq!(app.state().playback.speed, SPEED_MIN);

    let reset = app.handle_command(Command::SpeedReset);
    assert_eq!(
        audio_commands(&reset),
        vec![AudioCommand::SetSpeed(SPEED_DEFAULT)]
    );
    assert!(app.state().playback.speed == SPEED_DEFAULT);
}

#[test]
fn repeated_speed_steps_return_to_exact_unity() {
    let mut app = playing_fixture(0);

    for _ in 0..10 {
        app.handle_command(Command::SpeedUp);
    }
    for _ in 0..10 {
        app.handle_command(Command::SpeedDown);
    }

    assert_eq!(app.state().playback.speed, SPEED_DEFAULT);
    assert_eq!(app.state().playback.speed.tenths(), 10);
}

#[test]
fn repeated_forward_seek_emits_relative_commands_without_mutating_elapsed() {
    let mut app = playing_fixture(0);
    app.state.playback.duration = Some(Duration::from_secs(3600));
    app.state.playback.elapsed = Duration::from_secs(60);

    let forward = app.handle_command(Command::SeekForward);
    assert_eq!(
        audio_commands(&forward),
        vec![AudioCommand::SeekBy {
            forward: true,
            amount: Duration::from_secs(30),
        }]
    );
    assert_eq!(app.state().playback.elapsed, Duration::from_secs(60));

    let repeated = app.handle_command(Command::SeekForward);
    assert_eq!(
        audio_commands(&repeated),
        vec![AudioCommand::SeekBy {
            forward: true,
            amount: Duration::from_secs(30),
        }]
    );
    assert_eq!(app.state().playback.elapsed, Duration::from_secs(60));
}

#[test]
fn repeated_backward_seek_emits_relative_commands_without_mutating_elapsed() {
    let mut app = playing_fixture(0);
    app.state.playback.duration = Some(Duration::from_secs(120));
    app.state.playback.elapsed = Duration::from_secs(30);

    let backward = app.handle_command(Command::SeekBackward);
    assert_eq!(
        audio_commands(&backward),
        vec![AudioCommand::SeekBy {
            forward: false,
            amount: Duration::from_secs(5),
        }]
    );
    assert_eq!(app.state().playback.elapsed, Duration::from_secs(30));

    let repeated = app.handle_command(Command::SeekBackward);
    assert_eq!(
        audio_commands(&repeated),
        vec![AudioCommand::SeekBy {
            forward: false,
            amount: Duration::from_secs(5),
        }]
    );
    assert_eq!(app.state().playback.elapsed, Duration::from_secs(30));
}

#[test]
fn stale_progress_does_not_change_the_next_relative_seek_command() {
    let mut app = playing_fixture(0);
    app.state.playback.duration = Some(Duration::from_secs(3600));
    app.state.playback.elapsed = Duration::from_secs(60);

    let first = app.handle_command(Command::SeekForward);
    assert!(matches!(
        audio_commands(&first).as_slice(),
        [AudioCommand::SeekBy {
            forward: true,
            amount,
        }] if *amount == Duration::from_secs(30)
    ));

    // This snapshot was queued before the worker acknowledged the seek.
    app.apply_playback_progress(crate::audio::PlaybackSnapshot {
        status: PlayStatus::Playing,
        track_index: Some(0),
        elapsed: Duration::ZERO,
        duration: Some(Duration::from_secs(3600)),
        sink_health: crate::audio::SinkHealth::Healthy,
    });

    let second = app.handle_command(Command::SeekForward);
    assert!(matches!(
        audio_commands(&second).as_slice(),
        [AudioCommand::SeekBy {
            forward: true,
            amount,
        }] if *amount == Duration::from_secs(30)
    ));
}

#[test]
fn seek_without_an_active_track_stays_silent() {
    let mut app = App::new();

    let effects = app.handle_command(Command::SeekForward);

    assert!(effects.is_empty());
}

#[test]
fn play_selected_starts_the_cursor_entry() {
    let mut app = App::new();
    app.state
        .extend_playlist(["/x.flac", "/y.mp3"].map(PathBuf::from));
    app.state.playlist.select(1);

    let effects = app.handle_command(Command::PlaySelected);

    assert_eq!(
        audio_commands(&effects),
        vec![AudioCommand::Play {
            source: crate::stream::TrackSource::local("/y.mp3"),
            track_index: 1,
            generation: None,
        }]
    );
    assert_eq!(app.state().playback.track_index, Some(1));
}

#[test]
fn play_selected_on_an_empty_queue_only_notifies() {
    let mut app = App::new();

    let effects = app.handle_command(Command::PlaySelected);

    assert!(effects.is_empty());
    assert_eq!(
        app.state().notifications.last(),
        Some(&"Queue is empty".to_string())
    );
}

#[test]
fn cycle_repeat_walks_the_owner_mode_and_reports_each_step() {
    let mut app = playing_fixture(0);

    app.handle_command(Command::CycleRepeat);
    assert_eq!(
        app.state().playback_mode.repeat(),
        crate::playback_mode::RepeatMode::Track
    );

    app.handle_command(Command::CycleRepeat);
    assert_eq!(
        app.state().playback_mode.repeat(),
        crate::playback_mode::RepeatMode::All
    );

    app.handle_command(Command::CycleRepeat);
    assert_eq!(
        app.state().playback_mode.repeat(),
        crate::playback_mode::RepeatMode::Off
    );
    assert_eq!(
        app.state().notifications.last(),
        Some(&"Repeat mode: Off".to_string())
    );
}

#[test]
fn toggle_shuffle_flips_the_axis_and_reserves_the_playing_track() {
    let mut app = playing_fixture(1);

    let effects = app.handle_command(Command::ToggleShuffle);

    assert!(effects.is_empty(), "mode changes never touch the worker");
    assert!(app.state().playback_mode.shuffle());
    // The fresh pass keeps every entry drawable except the one playing
    assert!(app.state().navigation.unplayed_contains(0));
    assert!(!app.state().navigation.unplayed_contains(1));
    assert!(app.state().navigation.unplayed_contains(2));

    app.handle_command(Command::ToggleShuffle);
    assert!(!app.state().playback_mode.shuffle());
    assert_eq!(
        app.state().notifications.last(),
        Some(&"Shuffle off".to_string())
    );
}

#[test]
fn track_repeat_auto_advance_replays_the_same_entry() {
    let mut app = playing_fixture(0);
    app.handle_command(Command::CycleRepeat);

    let effects = app.apply_track_ended(0);

    assert_eq!(
        audio_commands(&effects),
        vec![AudioCommand::Play {
            source: crate::stream::TrackSource::local("/a.mp3"),
            track_index: 0,
            generation: None,
        }]
    );
    assert_eq!(app.state().playlist.cursor(), 0);
    assert_eq!(app.state().playback.status, PlayStatus::Playing);
}

#[test]
fn track_repeat_replays_the_playing_track_not_the_hovered_cursor() {
    // Track A is playing while the user moves the visual cursor to B. When
    // A finishes, repeat-track must replay A — the hovered row must not
    // influence playback — and the panel cursor should follow A.
    let mut app = playing_fixture(0);
    app.handle_command(Command::CycleRepeat); // RepeatMode::Track
    app.state.playlist.select(1); // hover over Track B, don't play it

    let effects = app.apply_track_ended(0); // Track A finished

    assert_eq!(
        audio_commands(&effects),
        vec![AudioCommand::Play {
            source: crate::stream::TrackSource::local("/a.mp3"),
            track_index: 0,
            generation: None,
        }],
        "repeat-track must replay the finished playing entry, not the hovered one"
    );
    assert_eq!(app.state().playlist.cursor(), 0, "cursor follows playback");
    assert_eq!(app.state().playback.track_index, Some(0));
}

#[test]
fn repeat_all_wraps_at_the_tail_for_manual_next() {
    let mut app = playing_fixture(2);
    app.handle_command(Command::CycleRepeat);
    app.handle_command(Command::CycleRepeat);

    let effects = app.handle_command(Command::NextTrack);

    assert_eq!(
        audio_commands(&effects),
        vec![AudioCommand::Play {
            source: crate::stream::TrackSource::local("/a.mp3"),
            track_index: 0,
            generation: None,
        }]
    );
    assert_eq!(app.state().playlist.cursor(), 0);
}

#[test]
fn shuffled_manual_next_never_repeats_back_to_back() {
    let mut app = App::new();
    app.state
        .extend_playlist(["/a.mp3", "/b.mp3", "/c.mp3", "/d.mp3"].map(PathBuf::from));
    app.state.playlist.select(0);
    app.state.playback.track_index = Some(0);
    app.handle_command(Command::ToggleShuffle);

    let mut visited = Vec::new();
    for _ in 0..8 {
        let effects = app.handle_command(Command::NextTrack);
        let commands = audio_commands(&effects);
        if let Some(AudioCommand::Play { track_index, .. }) = commands.first() {
            visited.push(*track_index);
        }
    }

    assert_eq!(visited.len(), 8, "shuffle plus manual skip always plays");
    for pair in visited.windows(2) {
        assert_ne!(
            pair[0], pair[1],
            "two consecutive draws repeated track {}",
            pair[0]
        );
    }
    assert!(
        visited.iter().all(|index| *index < 4),
        "every draw stayed inside the queue"
    );
}

#[test]
fn previous_follows_the_played_history_over_sequential_fallback() {
    let mut app = playing_fixture(0);
    // Walk forward once so the trail holds entry zero then entry one
    app.apply_track_ended(0);
    // Jump back to the first entry explicitly, making the trail
    // disagree with the plain sequential predecessor
    app.state.playlist.select(0);
    app.state.playback.elapsed = Duration::ZERO;
    app.handle_command(Command::PlaySelected);

    let effects = app.handle_command(Command::PreviousTrack);

    // History says entry one was heard just before, sequential
    // fallback from the first entry would have restarted instead
    assert_eq!(
        audio_commands(&effects),
        vec![AudioCommand::Play {
            source: crate::stream::TrackSource::local("/b.mp3"),
            track_index: 1,
            generation: None,
        }]
    );
    assert_eq!(app.state().playlist.cursor(), 1);
}

#[test]
fn delete_queue_entry_keeps_files_and_rebuilds_shuffle_safely() {
    let root = unique_temp_dir("delete-entry");
    let victim = root.join("victim.mp3");
    fs::write(&victim, b"audio").expect("fixture file");

    let mut app = App::new();
    app.state.extend_playlist([victim.clone()]);
    app.state
        .extend_playlist([PathBuf::from("/b.mp3"), PathBuf::from("/c.mp3")]);
    app.state.playlist.select(0);
    app.state.playback.track_index = Some(1);
    app.handle_command(Command::ToggleShuffle);

    let effects = app.handle_command(Command::DeleteQueueEntry);

    assert!(effects.is_empty(), "queue edits emit no worker commands");
    assert_eq!(app.state().playlist.len(), 2);
    assert!(
        victim.exists(),
        "deleting a queue entry never deletes files"
    );
    // The successor slid into the deleted row while the backing file stayed intact.
    assert_eq!(
        app.state().playlist.tracks()[0].path(),
        Some(Path::new("/b.mp3"))
    );
    assert_eq!(app.state().playback.track_index, Some(0));
    assert_eq!(app.state().navigation.history_len(), 0);
}

#[test]
fn deleting_the_playing_queue_entry_emits_stop_and_clears_playback() {
    let mut app = playing_fixture(1);
    app.state.persistence.last_track = Some(TrackLocation::local("/b.mp3"));
    app.state.active_playlist_name = Some("saved".to_string());
    app.state.async_ops.begin_stream_acquisition(
        TrackLocation::url(url::Url::parse("https://example.com/live").unwrap()),
        7,
    );
    app.state.playback.elapsed = Duration::from_secs(12);
    app.state.persistence.last_track_position_ms = 12_000;

    let effects = app.handle_command(Command::DeleteQueueEntry);

    assert_eq!(audio_commands(&effects), vec![AudioCommand::Stop]);
    assert!(effects.iter().any(|effect| matches!(
        effect,
        Effect::SaveActivePlaylist { name, .. } if name == "saved"
    )));
    assert_eq!(app.state().playback.status, PlayStatus::Stopped);
    assert_eq!(app.state().playback.track_index, None);
    assert_eq!(app.state().persistence.last_track, None);
    assert_eq!(app.state().persistence.last_track_position_ms, 0);
    assert!(app.state().async_ops.stream_activity().is_none());
    assert_eq!(app.state().playlist.cursor(), 1);
}

#[test]
fn delete_on_an_empty_queue_only_notifies() {
    let mut app = App::new();

    let effects = app.handle_command(Command::DeleteQueueEntry);

    assert!(effects.is_empty());
    assert_eq!(
        app.state().notifications.last(),
        Some(&"Queue is empty".to_string())
    );
}

#[test]
fn clear_queue_lets_the_running_track_finish_then_stops() {
    let root = unique_temp_dir("clear-queue");
    let keep = root.join("keep.mp3");
    fs::write(&keep, b"audio").expect("fixture file");

    let mut app = App::new();
    app.state.extend_playlist([keep.clone()]);
    app.state.playlist.select(0);
    app.state.playback.track_index = Some(0);
    app.state.playback.status = PlayStatus::Playing;

    let effects = app.handle_command(Command::ClearQueue);

    assert!(effects.is_empty(), "clearing never commands the worker");
    assert!(app.state().playlist.is_empty());
    assert!(keep.exists(), "clearing the queue must not touch files");
    assert_eq!(app.state().playback.status, PlayStatus::Playing);

    // The natural completion lands on an empty queue and stops
    let ended = app.apply_track_ended(0);
    assert!(ended.is_empty());
    assert_eq!(app.state().playback.status, PlayStatus::Stopped);
}

#[test]
fn swap_commands_move_rows_with_the_selection_riding_along() {
    let mut app = playing_fixture(1);
    app.state.playlist.select(2);

    let effects = app.handle_command(Command::SwapSelectedUp);

    assert!(effects.is_empty());
    assert_eq!(app.state().playlist.tracks()[1].display_name(), "c");
    assert_eq!(app.state().playlist.cursor(), 1);
    assert_eq!(app.state().playback.track_index, Some(2));
    assert_eq!(app.state().playback.status, PlayStatus::Playing);

    app.handle_command(Command::SwapSelectedDown);
    assert_eq!(app.state().playlist.tracks()[2].display_name(), "c");
    assert_eq!(app.state().playback.track_index, Some(1));
    assert_eq!(app.state().playback.status, PlayStatus::Playing);

    // The bottom boundary refuses silently without breaking state
    app.handle_command(Command::SwapSelectedDown);
    assert_eq!(app.state().playlist.tracks()[2].display_name(), "c");
}

#[test]
fn playlist_focus_routes_cursor_keys_to_the_queue_cursor() {
    let mut app = App::new();
    app.state
        .extend_playlist(["/a.mp3", "/b.mp3"].map(PathBuf::from));
    app.handle_command(Command::FocusNextPanel);

    app.handle_key_event(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE));

    assert_eq!(app.state().playlist.cursor(), 1);
    assert_eq!(app.state().browser.cursor(), 0, "browser stays untouched");

    app.handle_key_event(KeyEvent::new(KeyCode::Char('k'), KeyModifiers::NONE));
    assert_eq!(app.state().playlist.cursor(), 0);
}

#[test]
fn track_completions_auto_advance_until_the_queue_runs_out() {
    let mut app = playing_fixture(0);

    let first = app.apply_track_ended(0);
    assert_eq!(
        audio_commands(&first),
        vec![AudioCommand::Play {
            source: crate::stream::TrackSource::local("/b.mp3"),
            track_index: 1,
            generation: None,
        }]
    );

    let second = app.apply_track_ended(1);
    assert_eq!(
        audio_commands(&second),
        vec![AudioCommand::Play {
            source: crate::stream::TrackSource::local("/c.mp3"),
            track_index: 2,
            generation: None,
        }]
    );

    let last = app.apply_track_ended(2);
    assert!(last.is_empty(), "the tail stops instead of wrapping");
    assert_eq!(app.state().playback.status, PlayStatus::Stopped);
    assert_eq!(app.state().playback.elapsed, Duration::ZERO);
}

#[test]
fn stale_track_completions_are_ignored() {
    let mut app = playing_fixture(1);

    let effects = app.apply_track_ended(0);

    assert!(effects.is_empty());
    assert_eq!(app.state().playback.track_index, Some(1));
    assert_eq!(app.state().playback.status, PlayStatus::Playing);
}

#[test]
fn track_ended_for_a_removed_track_is_a_stale_no_op() {
    let mut app = App::new();
    app.state
        .extend_playlist(["/a.mp3", "/b.mp3"].map(PathBuf::from));
    app.state.playlist.select(0);
    app.state.playback.track_index = Some(0);
    app.state.playback.status = PlayStatus::Playing;
    app.state.persistence.last_track = Some(TrackLocation::local("/a.mp3"));

    // The user removes the playing track from the queue while it plays.
    app.state.playlist.remove_selected(&[0]);
    app.state.playback.track_index = None;

    // The worker reports the track finished, but it is no longer queued.
    let effects = app.apply_track_ended(0);

    assert!(
        effects.is_empty(),
        "a removed track must not advance anything"
    );
    assert_eq!(app.state().playback.status, PlayStatus::Playing);
}

#[test]
fn help_popup_opens_scrolls_and_closes_through_key_events() {
    let mut app = App::new();

    let ctrl_h = KeyEvent::new(KeyCode::Char('h'), KeyModifiers::CONTROL);
    app.handle_key_event(ctrl_h);
    assert_eq!(app.active_popup(), Some(Popup::Help { scroll: 0 }));

    // j and PageDown move the offset, and the popup stays open
    app.handle_key_event(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE));
    assert_eq!(app.active_popup(), Some(Popup::Help { scroll: 1 }));
    app.handle_key_event(KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE));
    assert_eq!(app.active_popup(), Some(Popup::Help { scroll: 11 }));

    // The opening key toggles back to closed
    app.handle_key_event(ctrl_h);
    assert_eq!(app.active_popup(), None);
}

#[test]
fn help_scroll_clamps_at_both_ends() {
    let mut app = App::new();
    app.handle_key_event(KeyEvent::new(KeyCode::Char('h'), KeyModifiers::CONTROL));

    // Scrolling up from the top stays at zero instead of wrapping
    app.handle_key_event(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
    assert_eq!(app.active_popup(), Some(Popup::Help { scroll: 0 }));

    app.handle_key_event(KeyEvent::new(KeyCode::End, KeyModifiers::NONE));
    let max_scroll = help_line_count(app.keys()).saturating_sub(1) as u16;
    assert_eq!(app.active_popup(), Some(Popup::Help { scroll: max_scroll }));

    // Paging past the end lands exactly on the last row
    app.handle_key_event(KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE));
    assert_eq!(app.active_popup(), Some(Popup::Help { scroll: max_scroll }));

    app.handle_key_event(KeyEvent::new(KeyCode::Home, KeyModifiers::NONE));
    assert_eq!(app.active_popup(), Some(Popup::Help { scroll: 0 }));
}

#[test]
fn help_popup_swallows_panel_commands_without_touching_state() {
    let mut app = playing_fixture(1);
    app.handle_key_event(KeyEvent::new(KeyCode::Char('h'), KeyModifiers::CONTROL));

    // Cursor moves scroll the popup and leave both panels untouched
    let effects = app.handle_key_event(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE));
    assert!(effects.is_empty());
    assert_eq!(app.active_popup(), Some(Popup::Help { scroll: 1 }));
    assert_eq!(app.state().browser.cursor(), 0);
    assert_eq!(app.state().playlist.cursor(), 1);

    // A playback command must never leak through the modal
    let effects = app.handle_key_event(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE));
    assert!(effects.is_empty());
    assert_eq!(app.state().playback.track_index, Some(1));
}

#[test]
fn quit_confirmation_keeps_its_dedicated_vocabulary() {
    let mut app = App::new();
    app.handle_command(Command::Quit);

    // The quit popup has no scrollable content, so a scroll command
    // produces no effects and leaves the dialog untouched. Key level
    // swallowing of everything but y/n/Esc lives in the input mapper
    // and is pinned by its own routing tests
    let effects = app.handle_command(Command::CursorDown);
    assert!(effects.is_empty());
    assert_eq!(app.active_popup(), Some(Popup::ConfirmQuit));

    app.handle_command(Command::ConfirmQuitYes);
    assert!(app.should_quit());
    assert_eq!(app.active_popup(), None);
}

#[test]
fn play_requests_artwork_only_when_the_feature_is_enabled() {
    let mut app = App::new();
    app.state
        .extend_playlist(["/x.flac", "/y.mp3"].map(PathBuf::from));
    app.state.playlist.select(1);
    app.state.artwork.set_enabled(true);

    let effects = app.handle_command(Command::PlaySelected);

    // Check that a LoadArtwork effect is present for the correct track
    let has_artwork = effects.iter().any(|e| {
        matches!(
            e,
            Effect::LoadArtwork {
                track_index: 1,
                path,
                ..
            } if path == &Some(PathBuf::from("/y.mp3"))
        )
    });
    assert!(
        has_artwork,
        "playing a track must also resolve its cover, got {effects:?}"
    );
    assert_eq!(audio_commands(&effects).len(), 1);

    // With the feature off the very same action emits no artwork work
    let mut disabled = App::new();
    disabled.state.extend_playlist([PathBuf::from("/x.flac")]);
    disabled.state.playlist.select(0);

    let effects = disabled.handle_command(Command::PlaySelected);
    assert_eq!(effects.len(), 1, "only the audio request may remain");
}

#[test]
fn auto_advance_requests_artwork_for_the_next_track_too() {
    let mut app = playing_fixture(0);
    app.state.artwork.set_enabled(true);

    let effects = app.apply_track_ended(0);

    let has_artwork = effects.iter().any(|e| {
        matches!(
            e,
            Effect::LoadArtwork {
                track_index: 1,
                path,
                ..
            } if path == &Some(PathBuf::from("/b.mp3"))
        )
    });
    assert!(
        has_artwork,
        "auto advance must behave like manual play, got {effects:?}"
    );
}

#[test]
fn artwork_results_are_dropped_once_the_track_moved_on() {
    let mut app = playing_fixture(1);
    app.state.artwork.set_enabled(true);

    // A late delivery for an entry the user already left is stale
    app.apply_artwork_loaded(
        0,
        Some(ArtworkProtocol::new(
            crate::artwork::testing::test_protocol(),
        )),
    );
    assert!(!app.state().artwork.has_artwork());
    assert_eq!(app.state().artwork.track_index(), None);

    // The same payload for the playing entry lands in the state
    app.apply_artwork_loaded(
        1,
        Some(ArtworkProtocol::new(
            crate::artwork::testing::test_protocol(),
        )),
    );
    assert!(app.state().artwork.has_artwork());
    assert_eq!(app.state().artwork.track_index(), Some(1));

    // A fresh miss clears the cell so the previous cover never sticks
    app.apply_artwork_loaded(1, None);
    assert!(!app.state().artwork.has_artwork());
    assert_eq!(app.state().artwork.track_index(), Some(1));
}

#[test]
fn artwork_keeps_track_a_visible_until_local_track_b_succeeds() {
    let mut app = playing_fixture(0);
    app.state.artwork.set_enabled(true);
    app.state.artwork.set_artwork(
        0,
        Some(ArtworkProtocol::new(
            crate::artwork::testing::test_protocol(),
        )),
    );
    app.state.playlist.select(1);

    let effects = app.begin_current_track();
    assert!(effects.iter().any(|effect| matches!(
        effect,
        Effect::LoadArtwork {
            track_index: 1,
            path: Some(path),
            ..
        } if path == &PathBuf::from("/b.mp3")
    )));
    assert!(app.state().artwork.has_artwork());
    assert_eq!(app.state().artwork.track_index(), Some(0));
    assert!(app.state().artwork.loading);

    app.apply_artwork_loaded(
        0,
        Some(ArtworkProtocol::new(
            crate::artwork::testing::test_protocol(),
        )),
    );
    assert!(app.state().artwork.has_artwork());
    assert_eq!(app.state().artwork.track_index(), Some(0));
    assert!(app.state().artwork.loading);

    app.apply_artwork_loaded(
        1,
        Some(ArtworkProtocol::new(
            crate::artwork::testing::test_protocol(),
        )),
    );
    assert!(app.state().artwork.has_artwork());
    assert_eq!(app.state().artwork.track_index(), Some(1));
    assert!(!app.state().artwork.loading);
}

#[test]
fn artwork_clears_track_a_when_local_track_b_has_no_usable_art() {
    let mut app = playing_fixture(0);
    app.state.artwork.set_enabled(true);
    app.state.artwork.set_artwork(
        0,
        Some(ArtworkProtocol::new(
            crate::artwork::testing::test_protocol(),
        )),
    );
    app.state.playlist.select(1);

    let effects = app.begin_current_track();
    assert!(effects.iter().any(|effect| matches!(
        effect,
        Effect::LoadArtwork {
            track_index: 1,
            path: Some(_),
            ..
        }
    )));
    assert!(app.state().artwork.has_artwork());

    app.apply_artwork_loaded(1, None);
    assert!(!app.state().artwork.has_artwork());
    assert_eq!(app.state().artwork.track_index(), Some(1));
    assert!(!app.state().artwork.loading);
}

#[test]
fn artwork_stream_request_keeps_track_a_until_remote_attempt_completes() {
    let url = url::Url::parse("https://radio.example.com/live").expect("valid URL");
    let mut stream = crate::track::Track::from_stream(url, crate::stream::StreamKind::Http);
    stream.set_metadata(crate::metadata::TrackMetadata {
        artist: "Artist".to_string(),
        album: "Album".to_string(),
        ..crate::metadata::TrackMetadata::default()
    });
    let mut app = App::new();
    app.state.playlist = {
        let mut playlist = crate::playlist::Playlist::new();
        playlist.extend([crate::track::Track::local("/a.mp3"), stream]);
        playlist
    };
    app.state.playlist.select(0);
    app.state.playback.track_index = Some(0);
    app.state.artwork.set_enabled(true);
    app.state.artwork.set_artwork(
        0,
        Some(ArtworkProtocol::new(
            crate::artwork::testing::test_protocol(),
        )),
    );
    app.state.playlist.select(1);

    let effects = app.begin_current_track();
    assert!(effects.iter().any(|effect| matches!(
        effect,
        Effect::LoadArtwork {
            track_index: 1,
            path: None,
            ..
        }
    )));
    assert!(app.state().artwork.has_artwork());
    assert_eq!(app.state().artwork.track_index(), Some(0));

    app.apply_artwork_loaded(
        1,
        Some(ArtworkProtocol::new(
            crate::artwork::testing::test_protocol(),
        )),
    );
    assert!(app.state().artwork.has_artwork());
    assert_eq!(app.state().artwork.track_index(), Some(1));
    assert!(!app.state().artwork.loading);
}

#[test]
fn artwork_stream_request_clears_track_a_when_remote_art_fails() {
    let url = url::Url::parse("https://radio.example.com/live").expect("valid URL");
    let mut stream = crate::track::Track::from_stream(url, crate::stream::StreamKind::Http);
    stream.set_metadata(crate::metadata::TrackMetadata {
        artist: "Artist".to_string(),
        album: "Album".to_string(),
        ..crate::metadata::TrackMetadata::default()
    });
    let mut app = App::new();
    app.state.playlist = {
        let mut playlist = crate::playlist::Playlist::new();
        playlist.extend([crate::track::Track::local("/a.mp3"), stream]);
        playlist
    };
    app.state.playlist.select(0);
    app.state.playback.track_index = Some(0);
    app.state.artwork.set_enabled(true);
    app.state.artwork.set_artwork(
        0,
        Some(ArtworkProtocol::new(
            crate::artwork::testing::test_protocol(),
        )),
    );
    app.state.playlist.select(1);

    let effects = app.begin_current_track();
    assert!(effects.iter().any(|effect| matches!(
        effect,
        Effect::LoadArtwork {
            track_index: 1,
            path: None,
            ..
        }
    )));
    assert!(app.state().artwork.has_artwork());
    assert!(app.state().artwork.loading);

    app.apply_artwork_loaded(1, None);
    assert!(!app.state().artwork.has_artwork());
    assert_eq!(app.state().artwork.track_index(), Some(1));
    assert!(!app.state().artwork.loading);
}

#[test]
fn stale_artwork_results_cannot_replace_a_pending_newer_track() {
    let mut app = playing_fixture(0);
    app.state.artwork.set_enabled(true);
    app.state.artwork.set_artwork(
        0,
        Some(ArtworkProtocol::new(
            crate::artwork::testing::test_protocol(),
        )),
    );

    app.state.playlist.select(1);
    app.begin_current_track();
    app.state.playlist.select(2);
    app.begin_current_track();

    app.apply_artwork_loaded(1, None);
    assert!(app.state().artwork.has_artwork());
    assert_eq!(app.state().artwork.track_index(), Some(0));
    assert!(app.state().artwork.loading);

    app.apply_artwork_loaded(2, None);
    assert!(!app.state().artwork.has_artwork());
    assert_eq!(app.state().artwork.track_index(), Some(2));
    assert!(!app.state().artwork.loading);
}

#[test]
fn load_artwork_effect_round_trips_through_the_bus() {
    let root = unique_temp_dir("artwork-roundtrip");
    let track = root.join("song.mp3");
    fs::write(&track, b"audio").expect("track fixture");
    let cover = root.join("cover.png");
    fs::write(&cover, crate::artwork::testing::tiny_png()).expect("cover fixture");

    let mut services = AppServices::new().expect("services construction");
    services.set_artwork_loader(Some(crate::artwork::ArtworkLoader::halfblocks()));

    execute_effects(
        vec![Effect::LoadArtwork {
            track_index: 7,
            path: Some(track),
            metadata: None,
            source_config: crate::config::ArtworkSource::All,
            cache_dir: root.to_path_buf(),
        }],
        &services,
        &pending_counter(),
    );

    match services.events().recv_timeout(EVENT_WAIT) {
        Ok(AppEvent::ArtworkLoaded {
            track_index,
            operation_id,
            artwork,
            ..
        }) => {
            assert_eq!(track_index, 7);
            assert!(operation_id.get() > 0);
            assert!(artwork.is_some(), "a valid cover must encode");
        }
        Ok(event) => panic!("expected ArtworkLoaded, got {event:?}"),
        Err(error) => panic!("bus receive failed: {error}"),
    }

    services.shutdown();
}

#[test]
fn load_artwork_without_a_loader_resolves_to_nothing() {
    // Sessions with artwork disabled or no capable terminal never
    // install a loader, and the effect must not notify or crash
    let services = AppServices::new().expect("services construction");

    execute_effects(
        vec![Effect::LoadArtwork {
            track_index: 0,
            path: Some(PathBuf::from("/nope.mp3")),
            metadata: None,
            source_config: crate::config::ArtworkSource::All,
            cache_dir: PathBuf::new(),
        }],
        &services,
        &pending_counter(),
    );

    match services.events().recv_timeout(EVENT_WAIT) {
        Ok(AppEvent::ArtworkLoaded {
            track_index,
            artwork: None,
            ..
        }) => assert_eq!(track_index, 0),
        Ok(event) => panic!("expected empty ArtworkLoaded, got {event:?}"),
        Err(error) => panic!("completion did not arrive: {error}"),
    }

    services.shutdown();
}

#[test]
fn progress_snapshots_merge_into_playback_state() {
    let mut app = playing_fixture(1);

    app.apply_playback_progress(crate::audio::PlaybackSnapshot {
        status: PlayStatus::Paused,
        track_index: Some(1),
        elapsed: Duration::from_secs(33),
        duration: Some(Duration::from_secs(300)),
        sink_health: crate::audio::SinkHealth::Lost,
    });

    assert_eq!(app.state().playback.status, PlayStatus::Paused);
    assert_eq!(app.state().playback.elapsed, Duration::from_secs(33));
    assert_eq!(
        app.state().playback.duration,
        Some(Duration::from_secs(300))
    );
    // Volume stays under command control, snapshots never touch it
    assert_eq!(app.state().playback.volume_percent, volume(70));
}

#[test]
fn toggle_artwork_flips_visibility_and_notifies() {
    let mut app = App::new();
    assert!(app.state().artwork.is_visible());

    app.handle_command(Command::ToggleArtwork);
    assert!(!app.state().artwork.is_visible());
    assert_eq!(
        app.state().notifications.last(),
        Some(&"Artwork hidden".to_string())
    );

    app.handle_command(Command::ToggleArtwork);
    assert!(app.state().artwork.is_visible());
    assert_eq!(
        app.state().notifications.last(),
        Some(&"Artwork visible".to_string())
    );
}

#[test]
fn named_playlist_autosaves_after_scan_completion() {
    let mut app = App::new();
    app.state_mut().active_playlist_name = Some("Rock".to_string());

    let effects =
        app.apply_scan_completed(PathBuf::from("/music"), vec![PathBuf::from("/music/a.mp3")]);

    assert!(
        effects.iter().any(|effect| matches!(
            effect,
            Effect::SaveActivePlaylist { name, .. } if name == "Rock"
        )),
        "a named active playlist must autosave on queue mutation"
    );
}

#[test]
fn anonymous_playlist_is_never_autosaved() {
    let mut app = App::new();
    assert_eq!(app.state().active_playlist_name, None);

    let effects =
        app.apply_scan_completed(PathBuf::from("/music"), vec![PathBuf::from("/music/a.mp3")]);

    assert!(
        !effects
            .iter()
            .any(|effect| matches!(effect, Effect::SaveActivePlaylist { .. })),
        "an anonymous queue must never spawn an autosave effect"
    );
}

fn playlist_app(label: &str) -> (TestTempDir, App) {
    let dir = unique_temp_dir(label);
    let store = crate::playlist::PlaylistStore::for_dir(dir.path());
    let app = App::from_config_and_store(crate::config::KeysConfig::default(), store);
    (dir, app)
}

fn popup_cursor(app: &App) -> usize {
    match app.state().popup_dialog.active_popup_ref() {
        Some(crate::state::Popup::PlaylistManager { cursor, .. }) => *cursor,
        _ => panic!("expected the playlist manager popup to be open"),
    }
}

/// Complete the Settings-open theme request through the blocking effect
/// and typed event path used by the event loop.
fn complete_theme_load(app: &mut App, effects: &[Effect], services: &AppServices) {
    let load = effects
        .iter()
        .find(|effect| matches!(effect, Effect::LoadTheme { .. }))
        .cloned()
        .expect("Settings must request a theme load");
    execute_effects(vec![load], services, &app.state().async_ops.pending_effects);
    let event = services
        .events()
        .recv_timeout(EVENT_WAIT)
        .expect("theme load completion");
    let AppEvent::ThemeLoaded {
        operation_id,
        request_id,
        visit_id,
        themes_dir,
        name,
        theme_names,
        result,
        purpose,
    } = event
    else {
        panic!("expected ThemeLoaded completion");
    };
    assert!(services.accepts_operation_completion(operation_id));
    assert!(!matches!(purpose, ThemeLoadPurpose::SettingsPreview));
    assert!(!app.apply_theme_loaded(
        request_id,
        visit_id,
        themes_dir,
        name,
        theme_names,
        result,
        purpose,
    ));
    services.release_operation_event(Some(operation_id));
}

/// Complete playlist effects through the same typed reducer completions
/// used by the event loop, while keeping these unit tests deterministic.
fn complete_playlist_effects(app: &mut App, effects: Vec<Effect>) -> Vec<Effect> {
    let mut pending = effects;
    let mut terminal = Vec::new();
    while let Some(effect) = pending.pop() {
        match effect {
            Effect::ListPlaylistNames {
                request_id,
                request,
            } => {
                let names = app.playlist_store().list_names();
                pending.extend(app.apply_playlist_names_completed(
                    request_id,
                    request,
                    names.map_err(|error| WorkerError::new("playlist-list", error)),
                ));
            }
            Effect::SavePlaylistNamed {
                request_id,
                name,
                action,
                contents,
            } => {
                let result = app
                    .playlist_store()
                    .save_rendered(&playlist_name(&name), &contents)
                    .map_err(|error| WorkerError::new("playlist-save-named", error));
                pending.extend(app.apply_playlist_saved(request_id, name, action, result));
            }
            Effect::RenamePlaylistNamed {
                request_id,
                old_name,
                new_name,
                action,
            } => {
                let result = match app.playlist_store().list_names() {
                    Ok(names)
                        if names
                            .iter()
                            .any(|candidate| candidate == &new_name && old_name != *candidate) =>
                    {
                        PlaylistRenameResult::Conflict
                    }
                    Ok(_) => app
                        .playlist_store()
                        .rename_playlist(&playlist_name(&old_name), &playlist_name(&new_name))
                        .map(|_| PlaylistRenameResult::Success)
                        .unwrap_or_else(|error| {
                            PlaylistRenameResult::Failed(WorkerError::new(
                                "playlist-rename-named",
                                error,
                            ))
                        }),
                    Err(error) => PlaylistRenameResult::Failed(WorkerError::new(
                        "playlist-rename-named",
                        error,
                    )),
                };
                pending.extend(
                    app.apply_playlist_renamed(request_id, old_name, new_name, action, result),
                );
            }
            Effect::DeletePlaylistNamed {
                request_id,
                name,
                cursor,
                was_active,
            } => {
                let deletion = app
                    .playlist_store()
                    .delete(&playlist_name(&name))
                    .map(|_| ())
                    .map_err(|error| WorkerError::new("playlist-delete-named", error));
                let names = app
                    .playlist_store()
                    .list_names()
                    .map_err(|error| WorkerError::new("playlist-delete-named", error));
                pending.extend(app.apply_playlist_deleted(
                    request_id,
                    name,
                    cursor,
                    was_active,
                    PlaylistDeleteResult { deletion, names },
                ));
            }
            Effect::LoadPlaylistNamed { request_id, name } => {
                let result = app
                    .playlist_store()
                    .load(&playlist_name(&name))
                    .map_err(|error| WorkerError::new("playlist-load-named", error));
                pending.extend(app.apply_playlist_loaded(request_id, name, result));
            }
            other => terminal.push(other),
        }
    }
    terminal.reverse();
    terminal
}

#[test]
fn named_playlist_save_success_closes_dialog_and_refreshes_manager() {
    let (_dir, mut app) = playlist_app("playlist-save-success");
    app.state_mut()
        .popup_dialog
        .open_popup(Popup::PlaylistManager {
            cursor: 0,
            names: Vec::new(),
        });
    app.state_mut()
        .popup_dialog
        .open_dialog(DialogMode::SaveAs, "Rock".to_string(), None);

    let validation_effects =
        app.request_named_save_validation(PlaylistSaveAction::SaveAs, "Rock".to_string());
    let [
        Effect::ListPlaylistNames {
            request_id,
            request,
        },
    ] = validation_effects.as_slice()
    else {
        panic!("save validation must list names on a worker");
    };
    let save_effects =
        app.apply_playlist_names_completed(*request_id, request.clone(), Ok(Vec::new()));
    let [
        Effect::SavePlaylistNamed {
            request_id: save_request_id,
            name,
            action,
            ..
        },
    ] = save_effects.as_slice()
    else {
        panic!("a free name must dispatch the named save effect");
    };

    let effects = app.apply_playlist_saved(
        *save_request_id,
        name.clone(),
        *action,
        Ok(PathBuf::from("Rock.m3u8")),
    );
    assert!(effects.iter().any(|effect| matches!(
        effect,
        Effect::ListPlaylistNames {
            request: PlaylistNamesRequest::RefreshManager { .. },
            ..
        }
    )));
    assert_eq!(app.state().popup_dialog.dialog_mode_ref(), None);
    assert_eq!(app.state().active_playlist_name.as_deref(), Some("Rock"));
    assert!(matches!(
        app.state().popup_dialog.active_popup_ref(),
        Some(Popup::PlaylistManager { .. })
    ));
}

#[test]
fn named_playlist_save_error_preserves_dialog_input_and_popup() {
    let (_dir, mut app) = playlist_app("playlist-save-error");
    app.state_mut()
        .popup_dialog
        .open_dialog(DialogMode::SaveAs, "Rock".to_string(), None);
    let validation_effects =
        app.request_named_save_validation(PlaylistSaveAction::SaveAs, "Rock".to_string());
    let [
        Effect::ListPlaylistNames {
            request_id,
            request,
        },
    ] = validation_effects.as_slice()
    else {
        panic!("save validation must list names on a worker");
    };
    let save_effects =
        app.apply_playlist_names_completed(*request_id, request.clone(), Ok(Vec::new()));
    let [
        Effect::SavePlaylistNamed {
            request_id: save_request_id,
            name,
            action,
            ..
        },
    ] = save_effects.as_slice()
    else {
        panic!("a free name must dispatch the named save effect");
    };

    app.apply_playlist_saved(
        *save_request_id,
        name.clone(),
        *action,
        Err(WorkerError::message("playlist-save-named", "disk full")),
    );
    assert_eq!(
        app.state().popup_dialog.dialog_mode_value(),
        Some(DialogMode::SaveAs)
    );
    assert_eq!(app.state().popup_dialog.dialog_input_value(), "Rock");
    assert_eq!(
        app.state().popup_dialog.dialog_error(),
        Some("Save failed: disk full")
    );
}

#[test]
fn stale_playlist_name_completion_cannot_reopen_a_cancelled_dialog() {
    let (_dir, mut app) = playlist_app("playlist-save-stale");
    app.state_mut()
        .popup_dialog
        .open_dialog(DialogMode::SaveAs, String::new(), None);
    let validation_effects =
        app.request_named_save_validation(PlaylistSaveAction::SaveAs, "Rock".to_string());
    let [
        Effect::ListPlaylistNames {
            request_id,
            request,
        },
    ] = validation_effects.as_slice()
    else {
        panic!("save validation must list names on a worker");
    };
    app.state_mut().async_ops.playlist_request.cancel();

    let effects = app.apply_playlist_names_completed(
        *request_id,
        request.clone(),
        Ok(vec!["Rock".to_string()]),
    );
    assert!(effects.is_empty());
    assert_eq!(app.state().popup_dialog.active_popup_ref(), None);
    assert_eq!(
        app.state().popup_dialog.dialog_mode_value(),
        Some(DialogMode::SaveAs)
    );
}

#[test]
fn playlist_panel_navigation_scrolls_directionally() {
    use crate::state::Panel;

    let mut app = App::new();
    app.state_mut()
        .extend_playlist((0..20).map(|i| PathBuf::from(format!("/m/t{i}.mp3"))));
    app.state_mut().playlist_viewport_height = 8;
    app.state_mut().active_panel = Panel::Playlist;

    // Jump to the bottom: the window hugs the tail (20 - 8 = 12).
    app.handle_command(Command::CursorBottom);
    assert_eq!(app.state().playlist.cursor(), 19);
    assert_eq!(app.state().playlist_scroll_offset, 12);

    // Climb up one row: the cursor rises within the fixed tail window.
    app.handle_command(Command::CursorUp);
    assert_eq!(app.state().playlist.cursor(), 18);

    // Paging up moves the cursor by a viewport-minus-one step.
    app.handle_command(Command::PageUp);
    assert!(app.state().playlist.cursor() < 18);
    assert!(app.state().playlist.cursor() + 8 > app.state().playlist_scroll_offset);

    // Home returns to the head and re-anchors the window at zero.
    app.handle_command(Command::CursorTop);
    assert_eq!(app.state().playlist.cursor(), 0);
    assert_eq!(app.state().playlist_scroll_offset, 0);
}

#[test]
fn playlist_manager_popup_moves_cursor_with_arrows() {
    let mut app = App::new();
    app.state_mut()
        .popup_dialog
        .open_popup(crate::state::Popup::PlaylistManager {
            cursor: 0,
            names: vec!["A".to_string(), "B".to_string(), "C".to_string()],
        });

    app.handle_key_event(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
    assert_eq!(popup_cursor(&app), 1);
    app.handle_key_event(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
    assert_eq!(popup_cursor(&app), 2);
    app.handle_key_event(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
    assert_eq!(popup_cursor(&app), 1);

    // The cursor clamps at both ends instead of wrapping or underflowing
    let mut app = App::new();
    app.state_mut()
        .popup_dialog
        .open_popup(crate::state::Popup::PlaylistManager {
            cursor: 0,
            names: vec!["A".to_string()],
        });
    app.handle_key_event(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
    assert_eq!(popup_cursor(&app), 0);
    app.handle_key_event(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
    assert_eq!(popup_cursor(&app), 0);
}

#[test]
fn playlist_manager_scroll_state_follows_direction_and_top_bottom_jumps() {
    let mut app = App::new();
    app.state_mut()
        .popup_dialog
        .open_popup(crate::state::Popup::PlaylistManager {
            cursor: 0,
            names: (0..20)
                .map(|index| format!("Playlist {index:02}"))
                .collect(),
        });

    for _ in 0..12 {
        app.handle_command(Command::MovePlaylistManagerDown);
    }
    assert_eq!(popup_cursor(&app), 12);
    assert_eq!(app.state().popup_dialog.manager_scroll_offset(), 1);

    app.handle_command(Command::MovePlaylistManagerBottom);
    assert_eq!(popup_cursor(&app), 19);
    assert_eq!(app.state().popup_dialog.manager_scroll_offset(), 6);

    app.state_mut()
        .popup_dialog
        .replace_popup(crate::state::Popup::PlaylistManager {
            cursor: 12,
            names: (0..20)
                .map(|index| format!("Playlist {index:02}"))
                .collect(),
        });
    assert_eq!(app.state().popup_dialog.manager_scroll_offset(), 1);

    app.handle_command(Command::MovePlaylistManagerTop);
    assert_eq!(popup_cursor(&app), 0);
    assert_eq!(app.state().popup_dialog.manager_scroll_offset(), 0);

    app.handle_command(Command::MovePlaylistManagerBottom);
    assert!(app.state().popup_dialog.manager_scroll_offset() > 0);
    app.state_mut().popup_dialog.clear();
    assert_eq!(app.state().popup_dialog.manager_scroll_offset(), 0);
}

#[test]
fn playlist_manager_pages_use_viewport_step_and_directional_context() {
    let mut app = App::new();
    app.state_mut().popup_dialog.set_manager_viewport_height(5);
    app.state_mut()
        .popup_dialog
        .open_popup(crate::state::Popup::PlaylistManager {
            cursor: 0,
            names: (0..20)
                .map(|index| format!("Playlist {index:02}"))
                .collect(),
        });

    app.handle_key_event(KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE));
    assert_eq!(popup_cursor(&app), 4);
    assert_eq!(app.state().popup_dialog.manager_scroll_offset(), 2);

    app.handle_key_event(KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE));
    assert_eq!(popup_cursor(&app), 8);
    assert_eq!(app.state().popup_dialog.manager_scroll_offset(), 6);

    app.handle_key_event(KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE));
    assert_eq!(popup_cursor(&app), 4);
    assert_eq!(app.state().popup_dialog.manager_scroll_offset(), 2);

    app.handle_key_event(KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE));
    assert_eq!(popup_cursor(&app), 0);
    assert_eq!(app.state().popup_dialog.manager_scroll_offset(), 0);

    app.handle_key_event(KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE));
    assert_eq!(popup_cursor(&app), 0);
    app.handle_key_event(KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE));
    app.handle_key_event(KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE));
    app.handle_key_event(KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE));
    assert_eq!(popup_cursor(&app), 12);
    app.handle_key_event(KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE));
    assert_eq!(popup_cursor(&app), 16);
    app.handle_key_event(KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE));
    assert_eq!(popup_cursor(&app), 19);
    assert_eq!(app.state().popup_dialog.manager_scroll_offset(), 15);
}

#[test]
fn playlist_manager_pages_handle_empty_single_exact_and_degenerate_viewports() {
    let mut app = App::new();
    app.state_mut().popup_dialog.set_manager_viewport_height(0);
    app.state_mut()
        .popup_dialog
        .open_popup(crate::state::Popup::PlaylistManager {
            cursor: 0,
            names: (0..3).map(|index| format!("Playlist {index}")).collect(),
        });

    app.handle_key_event(KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE));
    assert_eq!(popup_cursor(&app), 1);
    assert_eq!(app.state().popup_dialog.manager_scroll_offset(), 1);
    app.handle_key_event(KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE));
    assert_eq!(popup_cursor(&app), 0);
    assert_eq!(app.state().popup_dialog.manager_scroll_offset(), 0);

    for names in [Vec::new(), vec!["Only".to_string()]] {
        app.state_mut()
            .popup_dialog
            .open_popup(crate::state::Popup::PlaylistManager { cursor: 0, names });
        app.handle_key_event(KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE));
        app.handle_key_event(KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE));
        assert_eq!(popup_cursor(&app), 0);
        assert_eq!(app.state().popup_dialog.manager_scroll_offset(), 0);
    }

    app.state_mut().popup_dialog.set_manager_viewport_height(5);
    app.state_mut()
        .popup_dialog
        .open_popup(crate::state::Popup::PlaylistManager {
            cursor: 0,
            names: (0..5).map(|index| format!("Playlist {index}")).collect(),
        });
    app.handle_key_event(KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE));
    assert_eq!(popup_cursor(&app), 4);
    assert_eq!(app.state().popup_dialog.manager_scroll_offset(), 0);
    app.handle_key_event(KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE));
    assert_eq!(popup_cursor(&app), 0);
}

#[test]
fn loading_a_different_playlist_stops_and_requests_metadata_without_reordering() {
    use crate::config::{PlaylistColumnsConfig, SortBy};
    use crate::metadata::TrackMetadata;
    use crate::playlist::Playlist;
    use crate::track::Track;

    let (_dir, mut app) = playlist_app("manager-load-switch");
    let first_path = PathBuf::from("/music/first.mp3");
    let second_path = PathBuf::from("/music/second.mp3");
    let mut first = Track::local(first_path.clone());
    first.set_metadata(TrackMetadata {
        title: "Provisional first".to_string(),
        title_tagged: true,
        ..TrackMetadata::default()
    });
    let mut second = Track::local(second_path.clone());
    second.set_metadata(TrackMetadata {
        title: "Provisional second".to_string(),
        title_tagged: true,
        ..TrackMetadata::default()
    });
    let mut target = Playlist::new();
    target.extend([second, first]);
    app.playlist_store()
        .save(&playlist_name("Target"), &target)
        .unwrap();

    app.state_mut().active_playlist_name = Some("Current".to_string());
    app.state_mut().extend_playlist([PathBuf::from("/old.mp3")]);
    app.state_mut().playback.status = PlayStatus::Playing;
    app.state_mut().playback.track_index = Some(0);
    app.state_mut().playlist_viewport_height = 1;
    app.state_mut().playlist_scroll_offset = 8;
    app.state_mut().navigation.record_started(0);
    app.state_mut().playlist_columns = PlaylistColumnsConfig {
        display_by: SortBy::Metadata,
        metadata_artist: true,
        metadata_album: true,
        metadata_track_number: true,
    };
    app.state_mut()
        .popup_dialog
        .open_popup(Popup::PlaylistManager {
            cursor: 0,
            names: vec!["Target".to_string()],
        });

    let effects = app.handle_command(Command::LoadPlaylist);

    let [Effect::LoadPlaylistNamed { request_id, name }] = effects.as_slice() else {
        panic!("loading a playlist must dispatch a worker effect");
    };
    let loaded = app.playlist_store().load(&playlist_name(name)).unwrap();
    let effects = app.apply_playlist_loaded(*request_id, name.clone(), Ok(loaded));

    assert_eq!(
        effects,
        vec![
            Effect::Audio(AudioCommand::Stop),
            Effect::LoadMetadata(vec![second_path.clone(), first_path.clone()]),
        ]
    );
    assert_eq!(app.state().active_playlist_name.as_deref(), Some("Target"));
    assert_eq!(app.state().playback.status, PlayStatus::Stopped);
    assert_eq!(app.state().playback.track_index, None);
    assert_eq!(app.state().playlist.cursor(), 0);
    assert_eq!(app.state().navigation.history_len(), 0);
    assert_eq!(app.state().playlist_scroll_offset, 0);
    assert!(
        app.state()
            .playlist
            .tracks()
            .iter()
            .all(|track| track.metadata().is_some())
    );
    assert!(app.state().popup_dialog.active_popup_ref().is_none());
    assert!(
        !effects
            .iter()
            .any(|effect| matches!(effect, Effect::SaveActivePlaylist { .. }))
    );

    let metadata = |artist: &str, album: &str, number: u32, title: &str| TrackMetadata {
        title: title.to_string(),
        title_tagged: true,
        artist: artist.to_string(),
        album: album.to_string(),
        track_number: Some(number),
        ..TrackMetadata::default()
    };
    app.apply_metadata_completed(
        vec![
            (
                second_path.clone(),
                metadata("Artist B", "Album B", 2, "Title B"),
            ),
            (
                first_path.clone(),
                metadata("Artist A", "Album A", 1, "Title A"),
            ),
        ],
        0,
    );

    let labels: Vec<String> = app
        .state()
        .playlist
        .tracks()
        .iter()
        .map(|track| crate::playlist::sorter::display_label(track, &app.state().playlist_columns))
        .collect();
    assert_eq!(
        labels,
        [
            "Artist B - Album B - 2 - Title B",
            "Artist A - Album A - 1 - Title A"
        ]
    );
    assert_eq!(
        app.state().playlist.tracks()[0].display_location(),
        "/music/second.mp3",
        "metadata arrival must not reorder the restored queue"
    );
}

#[test]
fn loading_the_active_playlist_only_closes_the_manager() {
    use crate::config::{PlaylistColumnsConfig, SortBy};
    use crate::playlist::Playlist;
    use crate::track::Track;

    let (_dir, mut app) = playlist_app("manager-load-same");
    let path = PathBuf::from("/music/current.mp3");
    let mut current = Playlist::new();
    current.extend([Track::local(path)]);
    app.playlist_store()
        .save(&playlist_name("Current"), &current)
        .unwrap();
    app.state_mut().playlist = current;
    app.state_mut().active_playlist_name = Some("Current".to_string());
    app.state_mut().playback.status = PlayStatus::Playing;
    app.state_mut().playback.track_index = Some(0);
    app.state_mut().playback.elapsed = Duration::from_secs(12);
    app.state_mut().playlist_columns = PlaylistColumnsConfig {
        display_by: SortBy::Metadata,
        metadata_artist: true,
        metadata_album: true,
        metadata_track_number: true,
    };
    let before_playlist = app.state().playlist.clone();
    let before_pointer = app.state().playlist.tracks().as_ptr();
    let before_playback = app.state().playback.clone();
    let before_columns = app.state().playlist_columns.clone();
    let before_label = crate::playlist::sorter::display_label(
        &app.state().playlist.tracks()[0],
        &app.state().playlist_columns,
    );
    app.state_mut()
        .popup_dialog
        .open_popup(Popup::PlaylistManager {
            cursor: 0,
            names: vec!["Current".to_string()],
        });

    // The saved file is deliberately removed: selecting the active name
    // must not reload it at all.
    app.playlist_store()
        .delete(&playlist_name("Current"))
        .unwrap();
    let effects = app.handle_command(Command::LoadPlaylist);

    assert!(effects.is_empty());
    assert!(app.state().popup_dialog.active_popup_ref().is_none());
    assert_eq!(app.state().playlist, before_playlist);
    assert_eq!(app.state().playlist.tracks().as_ptr(), before_pointer);
    assert_eq!(app.state().playback, before_playback);
    assert_eq!(app.state().playlist_columns, before_columns);
    assert_eq!(
        crate::playlist::sorter::display_label(
            &app.state().playlist.tracks()[0],
            &app.state().playlist_columns,
        ),
        before_label
    );
}

#[test]
fn failed_playlist_load_preserves_queue_and_playback() {
    use crate::playlist::Playlist;
    use crate::track::Track;

    let (_dir, mut app) = playlist_app("manager-load-failure");
    let mut current = Playlist::new();
    current.extend([Track::local("/music/current.mp3")]);
    app.state_mut().playlist = current.clone();
    app.state_mut().active_playlist_name = Some("Current".to_string());
    app.state_mut().playback.status = PlayStatus::Playing;
    app.state_mut().playback.track_index = Some(0);
    app.state_mut().playback.elapsed = Duration::from_secs(9);
    let before_pointer = app.state().playlist.tracks().as_ptr();
    let before_playback = app.state().playback.clone();
    app.state_mut()
        .popup_dialog
        .open_popup(Popup::PlaylistManager {
            cursor: 0,
            names: vec!["Missing".to_string()],
        });

    let effects = app.handle_command(Command::LoadPlaylist);
    let [Effect::LoadPlaylistNamed { request_id, name }] = effects.as_slice() else {
        panic!("loading a playlist must dispatch a worker effect");
    };
    assert!(
        app.apply_playlist_loaded(
            *request_id,
            name.clone(),
            Err(WorkerError::message(
                "playlist-load-named",
                "No such file or directory",
            )),
        )
        .is_empty()
    );

    assert_eq!(app.state().playlist, current);
    assert_eq!(app.state().playlist.tracks().as_ptr(), before_pointer);
    assert_eq!(app.state().active_playlist_name.as_deref(), Some("Current"));
    assert_eq!(app.state().playback, before_playback);
    assert!(matches!(
        app.state().popup_dialog.active_popup_ref(),
        Some(Popup::PlaylistManager { .. })
    ));
    assert!(
        app.state()
            .notifications
            .last()
            .is_some_and(|message| message.contains("Could not load Missing"))
    );
}

#[test]
fn naming_dialog_captures_typed_text_and_backspace() {
    let mut app = App::new();
    let effects = app.handle_command(Command::SaveAsPlaylist);
    complete_playlist_effects(&mut app, effects);
    assert!(app.state().popup_dialog.dialog_mode_ref().is_some());
    // SaveAs prefills the default name so the user can edit it in place
    assert!(
        app.state()
            .popup_dialog
            .dialog_input_value()
            .starts_with("Playlist 1")
    );

    app.state_mut().popup_dialog.set_dialog_input(String::new());
    app.handle_key_event(KeyEvent::new(KeyCode::Char('R'), KeyModifiers::NONE));
    app.handle_key_event(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::NONE));
    app.handle_key_event(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE));
    assert_eq!(app.state().popup_dialog.dialog_input_value(), "Roc");

    app.handle_key_event(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE));
    assert_eq!(app.state().popup_dialog.dialog_input_value(), "Ro");
}

#[test]
fn save_as_prefills_the_active_playlist_name() {
    let dir = unique_temp_dir("save-as-prefill");
    let store = crate::playlist::PlaylistStore::for_dir(dir.path());
    store
        .save(&playlist_name("Rock"), &crate::playlist::Playlist::new())
        .unwrap();
    let mut app = App::from_config_and_store(crate::config::KeysConfig::default(), store);
    app.state_mut().active_playlist_name = Some("Rock".to_string());

    app.handle_command(Command::SaveAsPlaylist);

    // SaveAs reuses the running playlist name so the user edits in place
    assert_eq!(app.state().popup_dialog.dialog_input_value(), "Rock");
}

#[test]
fn naming_dialog_escape_cancels_without_saving() {
    let mut app = App::new();
    app.handle_command(Command::SaveAsPlaylist);
    app.handle_key_event(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE));
    app.handle_key_event(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));

    assert!(app.state().popup_dialog.dialog_mode_ref().is_none());
    assert_eq!(app.state().popup_dialog.dialog_input_value(), "");
    assert_eq!(app.state().active_playlist_name, None);
}

#[test]
fn naming_dialog_enter_confirms_a_save_as() {
    let dir = unique_temp_dir("dialog-confirm");
    let store = crate::playlist::PlaylistStore::for_dir(dir.path());
    store
        .save(&playlist_name("Rock"), &crate::playlist::Playlist::new())
        .unwrap();
    let mut app = App::from_config_and_store(crate::config::KeysConfig::default(), store);

    let effects = app.handle_command(Command::SaveAsPlaylist);
    complete_playlist_effects(&mut app, effects);
    app.state_mut().popup_dialog.set_dialog_input(String::new());
    app.handle_key_event(KeyEvent::new(KeyCode::Char('J'), KeyModifiers::NONE));
    app.handle_key_event(KeyEvent::new(KeyCode::Char('z'), KeyModifiers::NONE));
    let effects = app.handle_key_event(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    complete_playlist_effects(&mut app, effects);

    assert_eq!(app.state().popup_dialog.dialog_mode_ref(), None);
    assert_eq!(app.state().active_playlist_name.as_deref(), Some("Jz"));
    assert!(
        app.playlist_store().load(&playlist_name("Jz")).is_ok(),
        "the typed name is persisted"
    );
}

#[test]
fn d_key_deletes_the_selected_playlist_when_manager_is_open() {
    let (_dir, mut app) = playlist_app("keymap-d-open");
    app.playlist_store()
        .save(&playlist_name("Rock"), &crate::playlist::Playlist::new())
        .unwrap();
    app.playlist_store()
        .save(&playlist_name("Jazz"), &crate::playlist::Playlist::new())
        .unwrap();
    app.state_mut().active_playlist_name = Some("Rock".to_string());
    // Names are sorted, so cursor 0 selects "Jazz"
    let names = app.playlist_store().list_names().expect("playlist names");
    app.state_mut()
        .popup_dialog
        .open_popup(crate::state::Popup::PlaylistManager { cursor: 0, names });

    app.handle_key_event(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE));

    assert!(
        matches!(
            app.state().popup_dialog.active_popup_ref(),
            Some(crate::state::Popup::ConfirmDelete { name, .. }) if name == "Jazz"
        ),
        "pressing d must open the delete confirmation, not delete immediately"
    );
    assert!(
        app.playlist_store().load(&playlist_name("Jazz")).is_ok(),
        "the playlist must survive until the confirmation is accepted"
    );

    let effects = app.handle_key_event(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    complete_playlist_effects(&mut app, effects);

    assert!(
        app.playlist_store().load(&playlist_name("Jazz")).is_err(),
        "selection deleted after confirming the warning"
    );
    assert_eq!(
        app.state().active_playlist_name.as_deref(),
        Some("Rock"),
        "the active playlist is untouched"
    );
}

#[test]
fn d_key_deletes_the_playing_playlist_when_manager_is_closed() {
    let (_dir, mut app) = playlist_app("keymap-d-closed");
    app.playlist_store()
        .save(&playlist_name("Rock"), &crate::playlist::Playlist::new())
        .unwrap();
    app.state_mut().active_playlist_name = Some("Rock".to_string());

    app.handle_key_event(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE));

    assert!(
        matches!(
            app.state().popup_dialog.active_popup_ref(),
            Some(crate::state::Popup::ConfirmDelete { name, .. }) if name == "Rock"
        ),
        "pressing d must open the delete confirmation, not delete immediately"
    );
    assert!(
        app.playlist_store().load(&playlist_name("Rock")).is_ok(),
        "the playlist must survive until the confirmation is accepted"
    );

    let effects = app.handle_key_event(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    complete_playlist_effects(&mut app, effects);

    assert!(
        app.playlist_store().load(&playlist_name("Rock")).is_err(),
        "playing playlist removed after confirming the warning"
    );
    assert_eq!(
        app.state().active_playlist_name.as_deref(),
        Some("Playlist 1"),
        "deleting the playing playlist assigns the generated default name"
    );
    assert!(app.state().playlist.is_empty());
}

#[test]
fn open_playlist_manager_lists_saved_names() {
    let (_dir, mut app) = playlist_app("manager-open");
    app.playlist_store()
        .save(&playlist_name("Rock"), &crate::playlist::Playlist::new())
        .unwrap();

    let effects = app.handle_command(Command::OpenPlaylistManager);
    complete_playlist_effects(&mut app, effects);

    match app.state().popup_dialog.active_popup_ref() {
        Some(crate::state::Popup::PlaylistManager { names, .. }) => {
            assert_eq!(names, &vec!["Rock".to_string()]);
        }
        _ => panic!("OpenPlaylistManager must open the manager popup"),
    }
}

#[test]
fn open_playlist_manager_uses_natural_saved_playlist_order() {
    let (_dir, mut app) = playlist_app("manager-open-order");
    for name in [
        "Pearl Jam (copy 10)",
        "Pearl Jam (copy 2)",
        "Pearl Jam",
        "Pearl Jam (copy 1)",
    ] {
        app.playlist_store()
            .save(&playlist_name(name), &crate::playlist::Playlist::new())
            .expect("save playlist");
    }

    let effects = app.handle_command(Command::OpenPlaylistManager);
    complete_playlist_effects(&mut app, effects);

    match app.state().popup_dialog.active_popup_ref() {
        Some(crate::state::Popup::PlaylistManager { names, .. }) => {
            assert_eq!(
                names,
                &vec![
                    "Pearl Jam".to_string(),
                    "Pearl Jam (copy 1)".to_string(),
                    "Pearl Jam (copy 2)".to_string(),
                    "Pearl Jam (copy 10)".to_string(),
                ]
            );
        }
        _ => panic!("OpenPlaylistManager must open the manager popup"),
    }
}

#[test]
fn load_playlist_replaces_queue_and_marks_active() {
    let (_dir, mut app) = playlist_app("manager-load");
    app.playlist_store()
        .save(&playlist_name("Rock"), &crate::playlist::Playlist::new())
        .unwrap();
    let names = app.playlist_store().list_names().expect("playlist names");
    app.state_mut()
        .popup_dialog
        .open_popup(crate::state::Popup::PlaylistManager { cursor: 0, names });

    let effects = app.handle_command(Command::LoadPlaylist);
    let _ = complete_playlist_effects(&mut app, effects);

    assert_eq!(app.state().playlist.len(), 0);
    assert_eq!(app.state().active_playlist_name.as_deref(), Some("Rock"));
    assert!(
        app.state().popup_dialog.active_popup_ref().is_none(),
        "loading closes the popup"
    );
    assert_eq!(app.state().popup_dialog.manager_scroll_offset(), 0);
}

#[test]
fn save_as_refuses_to_overwrite_an_existing_name() {
    let (_dir, mut app) = playlist_app("manager-saveas");
    app.playlist_store()
        .save(&playlist_name("Rock"), &crate::playlist::Playlist::new())
        .unwrap();

    let effects = app.handle_command(Command::SaveAsPlaylist);
    complete_playlist_effects(&mut app, effects);
    assert_eq!(
        app.state().popup_dialog.dialog_mode_value(),
        Some(crate::state::DialogMode::SaveAs)
    );
    app.state_mut()
        .popup_dialog
        .set_dialog_input("Rock".to_string());

    let effects = app.handle_command(Command::ConfirmDialog);
    complete_playlist_effects(&mut app, effects);

    // A different-but-existing name must now ask for confirmation instead of
    // silently refusing, so the dialog stays open with no error.
    assert_eq!(
        app.state().popup_dialog.active_popup_value(),
        Some(crate::state::Popup::ConfirmOverwrite {
            name: "Rock".to_string()
        })
    );
    assert_eq!(app.state().popup_dialog.dialog_error(), None);
    assert_eq!(app.state().active_playlist_name, None);

    // Confirming the warning performs the overwrite.
    let effects = app.handle_command(Command::ConfirmDialog);
    complete_playlist_effects(&mut app, effects);
    assert_eq!(app.state().active_playlist_name.as_deref(), Some("Rock"));
    assert!(
        app.playlist_store().load(&playlist_name("Rock")).is_ok(),
        "the existing playlist is overwritten"
    );
    assert_eq!(app.state().popup_dialog.active_popup_ref(), None);
}

#[test]
fn save_as_same_name_overwrites_directly_without_warning() {
    let (_dir, mut app) = playlist_app("saveas-same");
    app.playlist_store()
        .save(&playlist_name("Rock"), &crate::playlist::Playlist::new())
        .unwrap();
    app.state_mut().active_playlist_name = Some("Rock".to_string());

    let effects = app.handle_command(Command::SaveAsPlaylist);
    complete_playlist_effects(&mut app, effects);
    app.state_mut()
        .popup_dialog
        .set_dialog_input("Rock".to_string());

    let effects = app.handle_command(Command::ConfirmDialog);
    complete_playlist_effects(&mut app, effects);

    // Re-saving the active playlist overwrites in place, no warning needed.
    assert_eq!(app.state().popup_dialog.active_popup_ref(), None);
    assert_eq!(app.state().popup_dialog.dialog_mode_ref(), None);
    assert_eq!(app.state().active_playlist_name.as_deref(), Some("Rock"));
    assert!(
        app.playlist_store().load(&playlist_name("Rock")).is_ok(),
        "the playlist is saved"
    );
}

#[test]
fn save_as_new_free_name_saves_directly_without_warning() {
    let (_dir, mut app) = playlist_app("saveas-new");
    app.playlist_store()
        .save(&playlist_name("Rock"), &crate::playlist::Playlist::new())
        .unwrap();

    let effects = app.handle_command(Command::SaveAsPlaylist);
    complete_playlist_effects(&mut app, effects);
    app.state_mut()
        .popup_dialog
        .set_dialog_input("BrandNew".to_string());

    let effects = app.handle_command(Command::ConfirmDialog);
    complete_playlist_effects(&mut app, effects);

    assert_eq!(app.state().popup_dialog.active_popup_ref(), None);
    assert_eq!(
        app.state().active_playlist_name.as_deref(),
        Some("BrandNew")
    );
    assert!(
        app.playlist_store()
            .load(&playlist_name("BrandNew"))
            .is_ok(),
        "the new playlist is saved"
    );
}

#[test]
fn save_as_overwrite_cancel_returns_to_naming_dialog() {
    let (_dir, mut app) = playlist_app("saveas-cancel");
    app.playlist_store()
        .save(&playlist_name("Rock"), &crate::playlist::Playlist::new())
        .unwrap();
    app.state_mut().active_playlist_name = Some("Jazz".to_string());

    let effects = app.handle_command(Command::SaveAsPlaylist);
    complete_playlist_effects(&mut app, effects);
    app.state_mut()
        .popup_dialog
        .set_dialog_input("Rock".to_string());

    let effects = app.handle_command(Command::ConfirmDialog);
    complete_playlist_effects(&mut app, effects);
    assert_eq!(
        app.state().popup_dialog.active_popup_value(),
        Some(crate::state::Popup::ConfirmOverwrite {
            name: "Rock".to_string()
        }),
        "conflict opens the overwrite warning"
    );

    app.handle_command(Command::CancelPopup);
    // Cancel drops the warning but keeps the naming dialog intact.
    assert_eq!(app.state().popup_dialog.active_popup_ref(), None);
    assert_eq!(
        app.state().popup_dialog.dialog_mode_value(),
        Some(crate::state::DialogMode::SaveAs)
    );
    assert_eq!(app.state().popup_dialog.dialog_input_value(), "Rock");
    assert_eq!(
        app.state().active_playlist_name.as_deref(),
        Some("Jazz"),
        "the active playlist is untouched"
    );
    // Nothing was overwritten.
    let original = app.playlist_store().load(&playlist_name("Rock")).unwrap();
    assert!(original.is_empty(), "the original Rock playlist is intact");
}

#[test]
fn overwrite_warning_requires_only_one_esc_to_return_to_naming_dialog() {
    let (_dir, mut app) = playlist_app("warn-esc");
    app.playlist_store()
        .save(&playlist_name("Rock"), &crate::playlist::Playlist::new())
        .unwrap();
    app.state_mut().active_playlist_name = Some("Jazz".to_string());

    let effects = app.handle_command(Command::SaveAsPlaylist);
    complete_playlist_effects(&mut app, effects);
    app.state_mut()
        .popup_dialog
        .set_dialog_input("Rock".to_string());
    let effects = app.handle_command(Command::ConfirmDialog);
    complete_playlist_effects(&mut app, effects);
    assert_eq!(
        app.state().popup_dialog.active_popup_value(),
        Some(crate::state::Popup::ConfirmOverwrite {
            name: "Rock".to_string()
        }),
        "conflict opens the overwrite warning"
    );
    assert_eq!(
        app.state().popup_dialog.dialog_mode_value(),
        Some(crate::state::DialogMode::SaveAs)
    );

    // The warning now owns its keys, so one Esc cancels the warning and
    // drops the user back into the naming dialog instead of closing it.
    let effects = app.handle_key_event(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    assert_eq!(
        app.state().popup_dialog.active_popup_ref(),
        None,
        "one Esc clears the warning popup"
    );
    assert_eq!(
        app.state().popup_dialog.dialog_mode_value(),
        Some(crate::state::DialogMode::SaveAs),
        "one Esc returns to the naming dialog"
    );
    assert_eq!(effects, Vec::new());

    // A second Esc now closes the naming dialog itself.
    app.handle_key_event(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    assert_eq!(
        app.state().popup_dialog.dialog_mode_value(),
        None,
        "second Esc cancels the dialog"
    );
}

#[test]
fn new_playlist_hotkey_opens_naming_dialog_with_next_name() {
    let (_dir, mut app) = playlist_app("newpl-open");
    app.playlist_store()
        .save(
            &playlist_name("Playlist 1"),
            &crate::playlist::Playlist::new(),
        )
        .unwrap();
    app.playlist_store()
        .save(
            &playlist_name("Playlist 2"),
            &crate::playlist::Playlist::new(),
        )
        .unwrap();
    app.playlist_store()
        .save(
            &playlist_name("Playlist 5"),
            &crate::playlist::Playlist::new(),
        )
        .unwrap();

    // Focus the queue panel so the global 'A' binding is active and not
    // swallowed by the browser's add-selection meaning.
    app.state_mut().active_panel = crate::state::Panel::Playlist;

    let effects = app.handle_key_event(KeyEvent::new(KeyCode::Char('A'), KeyModifiers::NONE));
    complete_playlist_effects(&mut app, effects);
    assert_eq!(
        app.state().popup_dialog.dialog_mode_value(),
        Some(crate::state::DialogMode::NewPlaylist)
    );
    assert_eq!(app.state().popup_dialog.dialog_input_value(), "Playlist 6");
}

#[test]
fn new_playlist_creates_empty_playlist_and_stops_playback() {
    let (_dir, mut app) = playlist_app("newpl-create");
    app.playlist_store()
        .save(
            &playlist_name("Playlist 1"),
            &crate::playlist::Playlist::new(),
        )
        .unwrap();
    app.playlist_store()
        .save(
            &playlist_name("Playlist 2"),
            &crate::playlist::Playlist::new(),
        )
        .unwrap();
    app.playlist_store()
        .save(
            &playlist_name("Playlist 5"),
            &crate::playlist::Playlist::new(),
        )
        .unwrap();

    let effects = app.handle_command(Command::NewPlaylist);
    complete_playlist_effects(&mut app, effects);
    assert_eq!(
        app.state().popup_dialog.dialog_mode_value(),
        Some(crate::state::DialogMode::NewPlaylist)
    );
    assert_eq!(app.state().popup_dialog.dialog_input_value(), "Playlist 6");

    app.state_mut()
        .popup_dialog
        .set_dialog_input("Fresh".to_string());
    let effects = app.handle_key_event(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    let effects = complete_playlist_effects(&mut app, effects);

    let saved = app
        .playlist_store()
        .load(&playlist_name("Fresh"))
        .expect("playlist saved");
    assert!(saved.is_empty(), "the new playlist is empty");
    assert_eq!(app.state().active_playlist_name.as_deref(), Some("Fresh"));
    assert!(app.state().playlist.is_empty(), "running playlist replaced");
    assert_eq!(
        app.state().playback.status,
        crate::audio::PlayStatus::Stopped
    );
    assert_eq!(effects, vec![Effect::Audio(AudioCommand::Stop)]);
    assert_eq!(app.state().popup_dialog.dialog_mode_ref(), None);
    assert_eq!(app.state().popup_dialog.active_popup_ref(), None);
    assert_eq!(
        app.active_panel(),
        Panel::Browser,
        "confirming a new playlist hands focus to the browser"
    );
}

#[test]
fn new_playlist_conflict_opens_overwrite_warning_then_creates() {
    let (_dir, mut app) = playlist_app("newpl-conflict");
    app.playlist_store()
        .save(
            &playlist_name("Existing"),
            &crate::playlist::Playlist::new(),
        )
        .unwrap();

    let effects = app.handle_command(Command::NewPlaylist);
    complete_playlist_effects(&mut app, effects);
    app.state_mut()
        .popup_dialog
        .set_dialog_input("Existing".to_string());

    // A name that already exists must ask before overwriting.
    let effects = app.handle_command(Command::ConfirmDialog);
    complete_playlist_effects(&mut app, effects);
    assert_eq!(
        app.state().popup_dialog.active_popup_value(),
        Some(crate::state::Popup::ConfirmOverwrite {
            name: "Existing".to_string()
        }),
        "conflict opens the overwrite warning"
    );
    assert_eq!(
        app.state().popup_dialog.dialog_mode_value(),
        Some(crate::state::DialogMode::NewPlaylist)
    );

    // Confirming the warning overwrites with an empty playlist, replaces the
    // running playlist and stops playback.
    let effects = app.handle_command(Command::ConfirmDialog);
    let effects = complete_playlist_effects(&mut app, effects);
    assert_eq!(app.state().popup_dialog.active_popup_ref(), None);
    assert_eq!(app.state().popup_dialog.dialog_mode_ref(), None);
    assert_eq!(
        app.state().active_playlist_name.as_deref(),
        Some("Existing")
    );
    let saved = app
        .playlist_store()
        .load(&playlist_name("Existing"))
        .expect("playlist saved");
    assert!(saved.is_empty(), "overwritten with an empty playlist");
    assert!(app.state().playlist.is_empty(), "running playlist replaced");
    assert_eq!(
        app.state().playback.status,
        crate::audio::PlayStatus::Stopped
    );
    assert_eq!(effects, vec![Effect::Audio(AudioCommand::Stop)]);
    assert_eq!(
        app.active_panel(),
        Panel::Browser,
        "confirming the overwrite also lands focus on the browser"
    );
}

#[test]
fn rename_playing_playlist_moves_the_saved_file() {
    let (_dir, mut app) = playlist_app("manager-rename");
    app.playlist_store()
        .save(&playlist_name("Rock"), &crate::playlist::Playlist::new())
        .unwrap();
    app.state_mut().active_playlist_name = Some("Rock".to_string());

    app.handle_command(Command::RenamePlaylist);
    assert_eq!(
        app.state().popup_dialog.dialog_mode_value(),
        Some(crate::state::DialogMode::RenamePlaying)
    );
    app.state_mut()
        .popup_dialog
        .set_dialog_input("Jazz".to_string());

    let effects = app.handle_command(Command::ConfirmDialog);
    complete_playlist_effects(&mut app, effects);

    assert_eq!(app.state().active_playlist_name.as_deref(), Some("Jazz"));
    assert!(
        app.playlist_store().load(&playlist_name("Jazz")).is_ok(),
        "new name exists"
    );
    assert!(
        app.playlist_store().load(&playlist_name("Rock")).is_err(),
        "old name gone"
    );
}

#[test]
fn named_playlist_rename_emits_a_worker_effect_before_touching_the_store() {
    let (_dir, mut app) = playlist_app("manager-rename-effect");
    app.playlist_store()
        .save(&playlist_name("Rock"), &crate::playlist::Playlist::new())
        .unwrap();
    app.state_mut().active_playlist_name = Some("Rock".to_string());
    app.handle_command(Command::RenamePlaylist);
    app.state_mut()
        .popup_dialog
        .set_dialog_input("Jazz".to_string());

    let effects = app.handle_command(Command::ConfirmDialog);
    assert!(matches!(
        effects.as_slice(),
        [Effect::RenamePlaylistNamed {
            old_name,
            new_name,
            action: PlaylistRenameAction::Playing,
            ..
        }] if old_name == "Rock" && new_name == "Jazz"
    ));
    assert!(app.playlist_store().load(&playlist_name("Rock")).is_ok());
    assert!(app.playlist_store().load(&playlist_name("Jazz")).is_err());
}

#[test]
fn named_playlist_rename_conflict_keeps_dialog_and_original_files() {
    let (_dir, mut app) = playlist_app("manager-rename-conflict");
    app.playlist_store()
        .save(&playlist_name("Rock"), &crate::playlist::Playlist::new())
        .unwrap();
    app.playlist_store()
        .save(&playlist_name("Jazz"), &crate::playlist::Playlist::new())
        .unwrap();
    app.state_mut().active_playlist_name = Some("Rock".to_string());
    app.handle_command(Command::RenamePlaylist);
    app.state_mut()
        .popup_dialog
        .set_dialog_input("Jazz".to_string());

    let effects = app.handle_command(Command::ConfirmDialog);
    complete_playlist_effects(&mut app, effects);

    assert_eq!(
        app.state().popup_dialog.dialog_mode_value(),
        Some(DialogMode::RenamePlaying)
    );
    assert_eq!(
        app.state().popup_dialog.dialog_error(),
        Some("Playlist Jazz already exists")
    );
    assert!(app.playlist_store().load(&playlist_name("Rock")).is_ok());
    assert!(app.playlist_store().load(&playlist_name("Jazz")).is_ok());
}

#[test]
fn stale_playlist_rename_completion_cannot_close_a_cancelled_dialog() {
    let (_dir, mut app) = playlist_app("manager-rename-stale");
    app.playlist_store()
        .save(&playlist_name("Rock"), &crate::playlist::Playlist::new())
        .unwrap();
    app.state_mut().active_playlist_name = Some("Rock".to_string());
    app.handle_command(Command::RenamePlaylist);
    app.state_mut()
        .popup_dialog
        .set_dialog_input("Jazz".to_string());
    let pending = app.handle_command(Command::ConfirmDialog);
    let [
        Effect::RenamePlaylistNamed {
            request_id,
            old_name,
            new_name,
            action,
        },
    ] = pending.as_slice()
    else {
        panic!("rename must dispatch a worker effect");
    };
    let request_id = *request_id;
    let old_name = old_name.clone();
    let new_name = new_name.clone();
    let action = *action;
    app.state_mut().async_ops.playlist_request.cancel();

    assert!(
        app.apply_playlist_renamed(
            request_id,
            old_name,
            new_name,
            action,
            PlaylistRenameResult::Success,
        )
        .is_empty()
    );
    assert_eq!(
        app.state().popup_dialog.dialog_mode_value(),
        Some(DialogMode::RenamePlaying)
    );
    assert_eq!(app.state().active_playlist_name.as_deref(), Some("Rock"));
}

#[test]
fn delete_with_popup_closed_drops_playing_playlist_and_queue() {
    let (_dir, mut app) = playlist_app("manager-del");
    app.playlist_store()
        .save(&playlist_name("Rock"), &crate::playlist::Playlist::new())
        .unwrap();
    app.state_mut().active_playlist_name = Some("Rock".to_string());

    app.handle_command(Command::DeletePlaylist);

    assert!(
        matches!(
            app.state().popup_dialog.active_popup_ref(),
            Some(crate::state::Popup::ConfirmDelete { name, .. }) if name == "Rock"
        ),
        "DeletePlaylist must open the confirmation, not delete immediately"
    );
    assert!(
        app.playlist_store().load(&playlist_name("Rock")).is_ok(),
        "the playlist file must survive until the confirmation is accepted"
    );

    let effects = app.handle_command(Command::ConfirmDialog);
    complete_playlist_effects(&mut app, effects);

    assert!(app.state().playlist.is_empty());
    assert_eq!(
        app.state().active_playlist_name.as_deref(),
        Some("Playlist 1"),
        "deleting the playing playlist assigns the generated default name"
    );
    assert!(
        app.playlist_store().load(&playlist_name("Rock")).is_err(),
        "the playing playlist file is removed"
    );
}

#[test]
fn named_playlist_delete_emits_a_worker_effect_without_touching_the_store() {
    let (_dir, mut app) = playlist_app("manager-delete-effect");
    app.playlist_store()
        .save(&playlist_name("Rock"), &crate::playlist::Playlist::new())
        .unwrap();
    app.state_mut().active_playlist_name = Some("Rock".to_string());
    app.handle_command(Command::DeletePlaylist);

    let effects = app.handle_command(Command::ConfirmDialog);
    assert!(matches!(
        effects.as_slice(),
        [Effect::DeletePlaylistNamed {
            name,
            cursor: None,
            was_active: true,
            ..
        }] if name == "Rock"
    ));
    assert!(app.playlist_store().load(&playlist_name("Rock")).is_ok());
}

#[test]
fn stale_playlist_delete_completion_cannot_replace_a_newer_popup() {
    let (_dir, mut app) = playlist_app("manager-delete-stale");
    app.playlist_store()
        .save(&playlist_name("Rock"), &crate::playlist::Playlist::new())
        .unwrap();
    app.state_mut().active_playlist_name = Some("Rock".to_string());
    app.handle_command(Command::DeletePlaylist);
    let pending = app.handle_command(Command::ConfirmDialog);
    let [
        Effect::DeletePlaylistNamed {
            request_id,
            name,
            cursor,
            was_active,
        },
    ] = pending.as_slice()
    else {
        panic!("delete must dispatch a worker effect");
    };
    let request_id = *request_id;
    let name = name.clone();
    let cursor = *cursor;
    let was_active = *was_active;
    let effects = app.handle_command(Command::CancelPopup);
    assert!(matches!(
        effects.as_slice(),
        [Effect::ListPlaylistNames {
            request: PlaylistNamesRequest::RefreshManager { .. },
            ..
        }]
    ));
    assert!(
        app.apply_playlist_deleted(
            request_id,
            name,
            cursor,
            was_active,
            PlaylistDeleteResult {
                deletion: Ok(()),
                names: Ok(Vec::new()),
            },
        )
        .is_empty()
    );
    assert!(matches!(
        app.state().popup_dialog.active_popup_value(),
        Some(Popup::PlaylistManager { .. })
    ));
    assert_eq!(app.state().active_playlist_name.as_deref(), Some("Rock"));
}

#[test]
fn failed_delete_keeps_the_queue_and_notifies_instead_of_reporting_success() {
    let (dir, mut app) = playlist_app("manager-del-fail");
    app.playlist_store()
        .save(&playlist_name("Rock"), &crate::playlist::Playlist::new())
        .unwrap();
    app.state_mut().active_playlist_name = Some("Rock".to_string());
    // Seed one track so we can tell whether the queue survived.
    app.state_mut()
        .extend_playlist(std::iter::once(PathBuf::from("/m/song.mp3")));

    // Force the delete to fail: replace the saved playlist file with a
    // directory so `fs::remove_file` errors (a directory cannot be removed
    // with remove_file on Linux).
    let path = dir.join("Rock.m3u8");
    std::fs::remove_file(&path).unwrap();
    std::fs::create_dir(&path).unwrap();

    app.handle_command(Command::DeletePlaylist);
    let effects = app.handle_command(Command::ConfirmDialog);
    complete_playlist_effects(&mut app, effects);

    // The delete failed, so the file must survive as a directory, the queue
    // must NOT be cleared, and the user must be told — not told it was
    // "deleted".
    assert!(path.is_dir(), "the playlist file cannot be removed");
    assert_eq!(
        app.state().playlist.len(),
        1,
        "queue must survive a failed delete"
    );
    assert_eq!(
        app.state().active_playlist_name.as_deref(),
        Some("Rock"),
        "the active playlist name is preserved on failure"
    );
    assert!(
        app.state()
            .notifications
            .iter()
            .any(|n| n.contains("Could not delete")),
        "a failed delete must surface an error notification"
    );
}

#[test]
fn delete_within_popup_only_removes_the_selection() {
    let (_dir, mut app) = playlist_app("manager-del-sel");
    app.playlist_store()
        .save(&playlist_name("Rock"), &crate::playlist::Playlist::new())
        .unwrap();
    app.playlist_store()
        .save(&playlist_name("Jazz"), &crate::playlist::Playlist::new())
        .unwrap();
    app.state_mut().active_playlist_name = Some("Rock".to_string());
    let names = app.playlist_store().list_names().expect("playlist names");
    // Names are sorted, so "Rock" is the last entry in the popup.
    let rock_index = names
        .iter()
        .position(|n| n == "Rock")
        .expect("Rock present");
    app.state_mut()
        .popup_dialog
        .open_popup(crate::state::Popup::PlaylistManager {
            cursor: rock_index,
            names,
        });

    app.handle_command(Command::DeletePlaylist);

    assert!(
        matches!(
            app.state().popup_dialog.active_popup_ref(),
            Some(crate::state::Popup::ConfirmDelete { name, .. }) if name == "Rock"
        ),
        "DeletePlaylist must open the confirmation, not delete immediately"
    );
    assert!(
        app.playlist_store().load(&playlist_name("Rock")).is_ok(),
        "the playlist file must survive until the confirmation is accepted"
    );

    let effects = app.handle_command(Command::ConfirmDialog);
    complete_playlist_effects(&mut app, effects);

    assert!(
        app.playlist_store().load(&playlist_name("Rock")).is_err(),
        "selected playlist is deleted after confirming the warning"
    );
    assert!(
        app.playlist_store().load(&playlist_name("Jazz")).is_ok(),
        "the other playlist is untouched"
    );
    assert_eq!(
        app.state().active_playlist_name,
        None,
        "deleting the active saved playlist clears the active name"
    );
}

#[test]
fn esc_cancels_the_delete_confirmation_and_keeps_the_playlist() {
    let (_dir, mut app) = playlist_app("manager-del-cancel");
    app.playlist_store()
        .save(&playlist_name("Rock"), &crate::playlist::Playlist::new())
        .unwrap();
    app.state_mut().active_playlist_name = Some("Rock".to_string());
    let names = app.playlist_store().list_names().expect("playlist names");
    app.state_mut()
        .popup_dialog
        .open_popup(crate::state::Popup::PlaylistManager { cursor: 0, names });

    app.handle_command(Command::DeletePlaylist);

    assert!(
        matches!(
            app.state().popup_dialog.active_popup_ref(),
            Some(crate::state::Popup::ConfirmDelete { name, .. }) if name == "Rock"
        ),
        "the delete confirmation must open before any change happens"
    );

    // Cancelling with Esc must restore the manager popup and leave the
    // playlist file on disk untouched.
    app.handle_key_event(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));

    assert!(
        matches!(
            app.state().popup_dialog.active_popup_ref(),
            Some(crate::state::Popup::PlaylistManager { .. })
        ),
        "Esc restores the playlist manager popup"
    );
    assert!(
        app.playlist_store().load(&playlist_name("Rock")).is_ok(),
        "cancelled confirmation must NOT delete the playlist file"
    );
    assert_eq!(
        app.state().active_playlist_name.as_deref(),
        Some("Rock"),
        "the active playlist survives a cancelled delete"
    );
}

#[test]
fn autosave_effect_persists_the_named_playlist_through_services() {
    let dir = unique_temp_dir("autosave-e2e");
    let mut services = AppServices::new().expect("services construct");
    services.set_playlist_store(Some(crate::playlist::PlaylistStore::for_dir(dir.path())));

    let effects = vec![Effect::SaveActivePlaylist {
        name: "Rock".to_string(),
        contents: "#EXTM3U\n".to_string(),
    }];
    execute_effects(effects, &services, &pending_counter());

    // The save runs on a blocking worker, so poll briefly instead of a
    // fixed sleep to keep the test free of needless latency
    let store = crate::playlist::PlaylistStore::for_dir(dir.path());
    let mut saved = false;
    for _ in 0..100 {
        if store.load(&playlist_name("Rock")).is_ok() {
            saved = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(saved, "the autosave effect must write the playlist file");

    services.shutdown();
}

#[test]
fn shift_c_opens_settings_and_esc_closes_every_tab() {
    let dir = unique_temp_dir("settings-esc-close");
    let mut app = App::new();
    app.set_config_paths(
        crate::config::AppConfig::default(),
        dir.to_path_buf(),
        dir.to_path_buf(),
    );
    // Shift+C opens settings
    app.handle_command(Command::OpenSettings);
    assert!(matches!(app.active_popup(), Some(Popup::Settings { .. })));

    for tab in [
        crate::state::SettingsTab::General,
        crate::state::SettingsTab::Appearance,
        crate::state::SettingsTab::Sound,
        crate::state::SettingsTab::Keys,
    ] {
        if let Some(Popup::Settings { tab: t, .. }) =
            app.state_mut().popup_dialog.active_popup_mut()
        {
            *t = tab;
        }
        app.handle_key_event(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(
            app.active_popup().is_none(),
            "Esc must close from tab {tab:?}"
        );
        // reopen for next iteration
        app.handle_command(Command::OpenSettings);
    }
}

#[test]
fn empty_browser_directory_is_rejected_on_commit() {
    let mut app = App::new();
    let dir = unique_temp_dir("settings-empty-dir");
    let valid = dir.to_string_lossy().into_owned();
    app.set_config_paths(
        crate::config::AppConfig::default(),
        dir.to_path_buf(),
        dir.to_path_buf(),
    );
    app.handle_command(Command::OpenSettings);
    // Set a valid directory first via draft
    if let Some(Popup::Settings { draft, .. }) = app.state.popup_dialog.active_popup_mut() {
        assert!(!draft.browser_directory.is_empty());
    }
    // Start editing browser_directory
    app.handle_key_event(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert_eq!(
        app.state.popup_dialog.dialog_mode_value(),
        Some(crate::state::DialogMode::SettingsEdit)
    );
    // Clear input to empty
    app.state.popup_dialog.set_dialog_input(String::new());
    let before = match &app.active_popup() {
        Some(Popup::Settings { draft, .. }) => draft.browser_directory.clone(),
        _ => String::new(),
    };
    // Try to commit empty
    app.handle_key_event(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    // Should revert and keep previous value
    let after = match &app.active_popup() {
        Some(Popup::Settings { draft, .. }) => draft.browser_directory.clone(),
        _ => String::new(),
    };
    assert_eq!(before, after, "empty directory must revert");
    assert!(
        app.state
            .popup_dialog
            .alert_message()
            .is_some_and(|m| m.contains("cannot be empty")),
        "must alert on empty"
    );
    // The editor stays open so the user can fix the value after dismissing.
    assert_eq!(
        app.state.popup_dialog.dialog_mode_value(),
        Some(crate::state::DialogMode::SettingsEdit)
    );
    assert!(app.state.popup_dialog.dialog_input_value().is_empty());
    // Dismissing the alert keeps the editor open for correction.
    app.handle_key_event(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    assert!(app.state.popup_dialog.alert_message().is_none());
    assert_eq!(
        app.state.popup_dialog.dialog_mode_value(),
        Some(crate::state::DialogMode::SettingsEdit)
    );
    let _ = valid;
}

#[test]
fn invalid_browser_path_is_rejected_via_alert() {
    let mut app = App::new();
    let dir = unique_temp_dir("settings-badpath");
    app.set_config_paths(
        crate::config::AppConfig::default(),
        dir.to_path_buf(),
        dir.to_path_buf(),
    );
    app.handle_command(Command::OpenSettings);
    app.handle_key_event(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)); // edit browser directory
    assert_eq!(
        app.state.popup_dialog.dialog_mode_value(),
        Some(crate::state::DialogMode::SettingsEdit)
    );
    let missing = dir.join("does-not-exist").to_string_lossy().into_owned();
    app.state.popup_dialog.set_dialog_input(missing.clone());
    let effects = app.handle_key_event(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert!(matches!(
        effects.as_slice(),
        [Effect::ValidateBrowserDirectory { .. }]
    ));
    assert!(
        app.state.popup_dialog.alert_message().is_none(),
        "validation must not inspect the filesystem on the reducer thread"
    );

    let services = AppServices::new().expect("services construction");
    execute_effects(effects, &services, &app.state().async_ops.pending_effects);
    let AppEvent::BrowserDirectoryValidated {
        operation_id,
        request_id,
        path,
        validation,
        result,
    } = services
        .events()
        .recv_timeout(EVENT_WAIT)
        .expect("validation result")
    else {
        panic!("expected browser directory validation result");
    };
    assert!(services.accepts_operation_completion(operation_id));
    let follow_up = app.apply_browser_directory_validated(request_id, path, validation, result);
    assert!(follow_up.is_empty());
    services.release_operation_event(Some(operation_id));
    services.shutdown();
    assert!(
        app.state
            .popup_dialog
            .alert_message()
            .is_some_and(|m| m.contains("Invalid path")),
        "invalid path must alert"
    );
    // Editor stays open with the typed value for correction.
    assert_eq!(
        app.state.popup_dialog.dialog_mode_value(),
        Some(crate::state::DialogMode::SettingsEdit)
    );
    assert!(
        app.state
            .popup_dialog
            .dialog_input_value()
            .contains("does-not-exist")
    );
}

#[test]
fn theme_custom_only_created_when_colors_differ() {
    let mut app = App::new();
    let dir = unique_temp_dir("settings-theme-diff");
    app.set_config_paths(
        crate::config::AppConfig::default(),
        dir.to_path_buf(),
        dir.to_path_buf(),
    );
    let services = AppServices::new().expect("services construction");
    let open_effects = app.handle_command(Command::OpenSettings);
    complete_theme_load(&mut app, &open_effects, &services);
    if let Some(Popup::Settings { tab, draft, .. }) =
        app.state_mut().popup_dialog.active_popup_mut()
    {
        *tab = crate::state::SettingsTab::Appearance;
        draft.appearance_theme_field = crate::state::SettingsField::AppearanceTheme(0);
        draft.appearance_column = crate::state::AppearanceColumn::Colors;
        draft.appearance_color_field = crate::ui::theme::ThemeColorField::Background;
    }
    let base = crate::ui::theme::Theme::default().to_colors().background;
    // Editing the color back to its original value must NOT create a custom file.
    app.handle_key_event(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    app.state.popup_dialog.set_dialog_input(base.clone());
    app.handle_key_event(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert!(!dir.join("themes/default-custom.toml").exists());
    // Editing to a different value must create the custom file.
    app.handle_key_event(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    app.state.popup_dialog.set_dialog_input("red".to_string());
    let save_effects = app.handle_key_event(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert!(matches!(
        save_effects.as_slice(),
        [Effect::SaveTheme { .. }]
    ));
    execute_effects(
        save_effects,
        &services,
        &app.state().async_ops.pending_effects,
    );
    let AppEvent::ThemeSaved {
        operation_id,
        request_id,
        visit_id,
        themes_dir,
        name,
        colors,
        result,
    } = services
        .events()
        .recv_timeout(EVENT_WAIT)
        .expect("theme save completion")
    else {
        panic!("expected ThemeSaved completion");
    };
    let follow_up = app.apply_theme_saved(request_id, visit_id, themes_dir, name, colors, result);
    services.release_operation_event(Some(operation_id));
    assert!(
        follow_up
            .0
            .iter()
            .any(|effect| matches!(effect, Effect::SaveConfig { .. }))
    );
    assert!(dir.join("themes/default-custom.toml").exists());
    services.shutdown();
}

#[test]
fn theme_load_completion_populates_names_palette_and_selected_alignment() {
    let root = unique_temp_dir("settings-theme-load");
    let themes_dir = root.path().join("themes");
    fs::create_dir_all(&themes_dir).expect("themes directory");
    let mut colors = crate::ui::theme::Theme::default().to_colors();
    colors.background = "red".to_string();
    crate::ui::theme::write_theme_file(&themes_dir, "solarized", &colors).expect("theme fixture");

    let mut config = crate::config::AppConfig::default();
    config.ui.theme = "solarized".to_string();
    let mut app = App::new();
    app.set_config_paths(config, root.path().to_path_buf(), root.path().join("data"));
    let services = AppServices::new().expect("services construction");
    let effects = app.handle_command(Command::OpenSettings);
    let load = effects
        .iter()
        .find(|effect| matches!(effect, Effect::LoadTheme { .. }))
        .cloned()
        .expect("theme load effect");
    execute_effects(
        vec![load],
        &services,
        &app.state().async_ops.pending_effects,
    );
    let AppEvent::ThemeLoaded {
        operation_id,
        request_id,
        visit_id,
        themes_dir,
        name,
        theme_names,
        result,
        purpose,
    } = services
        .events()
        .recv_timeout(EVENT_WAIT)
        .expect("theme load completion")
    else {
        panic!("expected ThemeLoaded completion");
    };
    assert!(services.accepts_operation_completion(operation_id));
    assert!(!app.apply_theme_loaded(
        request_id,
        visit_id,
        themes_dir,
        name,
        theme_names,
        result,
        purpose,
    ));
    let Some(Popup::Settings { draft, .. }) = app.active_popup_ref() else {
        panic!("Settings popup must remain open");
    };
    assert!(draft.theme_names.iter().any(|name| name == "solarized"));
    assert_eq!(
        draft.theme_names[draft.selected_theme().expect("theme selection")],
        "solarized"
    );
    assert_eq!(draft.colors.background, "Red");
    services.release_operation_event(Some(operation_id));
    services.shutdown();
}

#[test]
fn stale_theme_selection_and_visit_results_are_rejected() {
    let mut app = App::new();
    let first_open = app.handle_command(Command::OpenSettings);
    let Effect::LoadTheme {
        request_id: first_request,
        visit_id: first_visit,
        themes_dir: first_dir,
        name: first_name,
        ..
    } = first_open
        .iter()
        .find(|effect| matches!(effect, Effect::LoadTheme { .. }))
        .cloned()
        .expect("first theme load effect")
    else {
        unreachable!();
    };

    if let Some(Popup::Settings { tab, draft, .. }) =
        app.state_mut().popup_dialog.active_popup_mut()
    {
        *tab = SettingsTab::Appearance;
        draft.appearance_column = crate::state::AppearanceColumn::Themes;
        draft.theme_names = vec!["default".to_string(), "gruvbox".to_string()];
    }
    let selection_effects = app.handle_settings_key(Command::CursorDown);
    let Effect::LoadTheme {
        request_id: selection_request,
        visit_id: selection_visit,
        themes_dir: selection_dir,
        name: selection_name,
        purpose: selection_purpose,
        ..
    } = selection_effects
        .first()
        .cloned()
        .expect("selection theme load effect")
    else {
        panic!("expected a theme preview effect");
    };

    assert!(!app.apply_theme_loaded(
        first_request,
        first_visit,
        first_dir,
        first_name,
        Some(vec!["default".to_string()]),
        Ok(crate::ui::theme::Theme::default().to_colors()),
        ThemeLoadPurpose::SettingsOpen,
    ));
    let changed = crate::ui::theme::ThemeColors {
        background: "red".to_string(),
        ..crate::ui::theme::ThemeColors::default()
    };
    assert!(!app.apply_theme_loaded(
        selection_request,
        selection_visit,
        selection_dir,
        selection_name,
        None,
        Ok(changed.clone()),
        selection_purpose,
    ));
    assert_eq!(
        app.active_popup_ref().and_then(|popup| match popup {
            Popup::Settings { draft, .. } => Some(draft.colors.background.clone()),
            _ => None,
        }),
        Some("red".to_string())
    );

    let second_open = app.handle_command(Command::OpenSettings);
    let Effect::LoadTheme {
        request_id: second_request,
        visit_id: second_visit,
        themes_dir: second_dir,
        name: second_name,
        include_names: _,
        purpose: second_purpose,
        ..
    } = second_open
        .iter()
        .find(|effect| matches!(effect, Effect::LoadTheme { .. }))
        .cloned()
        .expect("second theme load effect")
    else {
        unreachable!();
    };
    assert!(!app.apply_theme_loaded(
        selection_request,
        selection_visit,
        PathBuf::new(),
        "gruvbox".to_string(),
        None,
        Ok(changed),
        ThemeLoadPurpose::SettingsPreview,
    ));
    assert!(!app.apply_theme_loaded(
        second_request,
        second_visit,
        second_dir,
        second_name,
        Some(vec!["default".to_string(), "gruvbox".to_string()]),
        Ok(crate::ui::theme::Theme::default().to_colors()),
        second_purpose,
    ));
}

#[test]
fn theme_save_failure_keeps_editable_draft_and_error() {
    let root = unique_temp_dir("settings-theme-save-failure");
    let mut app = App::new();
    app.set_config_paths(
        crate::config::AppConfig::default(),
        root.path().to_path_buf(),
        root.path().join("data"),
    );
    let services = AppServices::new().expect("services construction");
    let open_effects = app.handle_command(Command::OpenSettings);
    complete_theme_load(&mut app, &open_effects, &services);
    fs::write(root.path().join("themes"), b"not a directory").expect("blocking theme path");

    if let Some(Popup::Settings { tab, draft, .. }) =
        app.state_mut().popup_dialog.active_popup_mut()
    {
        *tab = SettingsTab::Appearance;
        draft.appearance_column = crate::state::AppearanceColumn::Colors;
        draft.appearance_color_field = crate::ui::theme::ThemeColorField::Background;
    }
    app.handle_key_event(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    app.state_mut()
        .popup_dialog
        .set_dialog_input("red".to_string());
    let effects = app.handle_key_event(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert!(matches!(effects.as_slice(), [Effect::SaveTheme { .. }]));
    execute_effects(effects, &services, &app.state().async_ops.pending_effects);
    let AppEvent::ThemeSaved {
        operation_id,
        request_id,
        visit_id,
        themes_dir,
        name,
        colors,
        result,
    } = services
        .events()
        .recv_timeout(EVENT_WAIT)
        .expect("theme save failure completion")
    else {
        panic!("expected ThemeSaved completion");
    };
    let _ = app.apply_theme_saved(request_id, visit_id, themes_dir, name, colors, result);
    assert_eq!(
        app.state().popup_dialog.dialog_mode_value(),
        Some(crate::state::DialogMode::SettingsEdit)
    );
    assert!(
        app.state()
            .popup_dialog
            .dialog_error()
            .is_some_and(|error| error.contains("Could not save theme"))
    );
    assert_eq!(
        app.active_popup_ref().and_then(|popup| match popup {
            Popup::Settings { draft, .. } => Some(draft.colors.background.as_str()),
            _ => None,
        }),
        Some("red")
    );
    services.release_operation_event(Some(operation_id));
    services.shutdown();
}

#[test]
fn duplicate_key_binding_is_rejected() {
    let mut app = App::new();
    app.handle_command(Command::OpenSettings);
    // Navigate to Keys tab
    if let Some(Popup::Settings { tab, .. }) = app.state_mut().popup_dialog.active_popup_mut() {
        *tab = crate::state::SettingsTab::Keys;
    }
    // Ensure cursor at 0 (quit)
    if let Some(Popup::Settings { draft, .. }) = app.state_mut().popup_dialog.active_popup_mut() {
        draft.keys_field = crate::config::KeySettingsRow::Cancel;
    }
    // Start editing cancel key
    app.handle_key_event(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert_eq!(
        app.state.popup_dialog.dialog_mode_value(),
        Some(crate::state::DialogMode::SettingsEdit)
    );
    // Try to set cancel to same as quit ("q")
    let quit_val = match &app.active_popup() {
        Some(Popup::Settings { draft, .. }) => draft.keys_draft.quit.as_str().to_string(),
        _ => "q".to_string(),
    };
    app.state.popup_dialog.set_dialog_input(quit_val.clone());
    app.handle_key_event(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert!(
        app.state
            .popup_dialog
            .alert_message()
            .is_some_and(|m| m.contains("conflicts")),
        "duplicate must alert via popup"
    );
    // Value must not have changed to duplicate (still original cancel)
    let cancel_after = match &app.active_popup() {
        Some(Popup::Settings { draft, .. }) => draft.keys_draft.cancel.clone(),
        _ => key("q"),
    };
    assert_ne!(cancel_after.as_str(), quit_val, "duplicate must revert");
    // Any key dismisses the alert without closing the settings popup.
    app.handle_key_event(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    assert!(app.state.popup_dialog.alert_message().is_none());
    assert!(matches!(app.active_popup(), Some(Popup::Settings { .. })));
}

#[test]
fn help_cache_tracks_a_committed_key_edit() {
    let mut app = App::new();
    app.handle_command(Command::OpenSettings);
    if let Some(Popup::Settings { tab, draft, .. }) =
        app.state_mut().popup_dialog.active_popup_mut()
    {
        *tab = SettingsTab::Keys;
        draft.keys_field = crate::config::KeySettingsRow::Quit;
    }

    app.handle_key_event(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    app.state_mut()
        .popup_dialog
        .set_dialog_input("Q".to_string());
    app.handle_key_event(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

    assert_eq!(app.keys().quit.as_str(), "Q");
    assert!(app.help_content().lines().iter().any(|line| matches!(
        line,
        crate::input::HelpContentLine::Row { key, description }
            if key.starts_with(" Q") && *description == "quit, asks for confirmation"
    )));
}

#[test]
fn tab_cycles_away_from_appearance() {
    let mut app = App::new();
    app.handle_command(Command::OpenSettings);
    if let Some(Popup::Settings { tab, .. }) = app.state_mut().popup_dialog.active_popup_mut() {
        *tab = crate::state::SettingsTab::Appearance;
    }
    app.handle_key_event(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
    assert!(matches!(
        app.active_popup(),
        Some(Popup::Settings {
            tab: crate::state::SettingsTab::Keys,
            ..
        })
    ));
    app.handle_key_event(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
    assert!(matches!(
        app.active_popup(),
        Some(Popup::Settings {
            tab: crate::state::SettingsTab::Playback,
            ..
        })
    ));
    app.handle_key_event(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
    assert!(matches!(
        app.active_popup(),
        Some(Popup::Settings {
            tab: crate::state::SettingsTab::Sound,
            ..
        })
    ));
    app.handle_key_event(KeyEvent::new(KeyCode::BackTab, KeyModifiers::NONE));
    assert!(matches!(
        app.active_popup(),
        Some(Popup::Settings {
            tab: crate::state::SettingsTab::Playback,
            ..
        })
    ));
    app.handle_key_event(KeyEvent::new(KeyCode::BackTab, KeyModifiers::NONE));
    assert!(matches!(
        app.active_popup(),
        Some(Popup::Settings {
            tab: crate::state::SettingsTab::Keys,
            ..
        })
    ));
}

#[test]
fn left_right_moves_appearance_columns() {
    let mut app = App::new();
    app.handle_command(Command::OpenSettings);
    if let Some(Popup::Settings { tab, .. }) = app.state_mut().popup_dialog.active_popup_mut() {
        *tab = crate::state::SettingsTab::Appearance;
        // Default column is now 1 (Theme); move through the 3-column cycle.
    }
    app.handle_key_event(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
    if let Some(Popup::Settings { draft, .. }) = &app.active_popup() {
        assert_eq!(
            draft.appearance_column,
            crate::state::AppearanceColumn::Colors
        );
    }
    app.handle_key_event(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
    if let Some(Popup::Settings { draft, .. }) = &app.active_popup() {
        assert_eq!(
            draft.appearance_column,
            crate::state::AppearanceColumn::Colors,
            "clamps at the last column"
        );
    }
    app.handle_key_event(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE));
    if let Some(Popup::Settings { draft, .. }) = &app.active_popup() {
        assert_eq!(
            draft.appearance_column,
            crate::state::AppearanceColumn::Themes
        );
    }
    app.handle_key_event(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE));
    if let Some(Popup::Settings { draft, .. }) = &app.active_popup() {
        assert_eq!(
            draft.appearance_column,
            crate::state::AppearanceColumn::Display
        );
    }
    app.handle_key_event(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE));
    if let Some(Popup::Settings { draft, .. }) = &app.active_popup() {
        assert_eq!(
            draft.appearance_column,
            crate::state::AppearanceColumn::Colors,
            "wraps back to the last column"
        );
    }
}

#[test]
fn hl_keys_navigate_appearance_columns() {
    let mut app = App::new();
    app.handle_command(Command::OpenSettings);
    if let Some(Popup::Settings { tab, draft, .. }) =
        app.state_mut().popup_dialog.active_popup_mut()
    {
        *tab = crate::state::SettingsTab::Appearance;
        draft.appearance_column = crate::state::AppearanceColumn::Display;
    }
    let right = KeyEvent::new(KeyCode::Char('l'), KeyModifiers::NONE);
    let left = KeyEvent::new(KeyCode::Char('h'), KeyModifiers::NONE);

    app.handle_key_event(right);
    if let Some(Popup::Settings { draft, .. }) = app.active_popup() {
        assert_eq!(
            draft.appearance_column,
            crate::state::AppearanceColumn::Themes,
            "l moves to the next column"
        );
    }
    app.handle_key_event(right);
    if let Some(Popup::Settings { draft, .. }) = app.active_popup() {
        assert_eq!(
            draft.appearance_column,
            crate::state::AppearanceColumn::Colors,
            "l clamps at the last column"
        );
    }
    app.handle_key_event(left);
    if let Some(Popup::Settings { draft, .. }) = app.active_popup() {
        assert_eq!(
            draft.appearance_column,
            crate::state::AppearanceColumn::Themes,
            "h moves to the previous column"
        );
    }
    app.handle_key_event(left);
    if let Some(Popup::Settings { draft, .. }) = app.active_popup() {
        assert_eq!(
            draft.appearance_column,
            crate::state::AppearanceColumn::Display,
            "h back to the fixed Options column"
        );
    }
}

#[test]
fn enter_on_theme_applies_and_queues_reload() {
    let dir = unique_temp_dir("settings-theme-apply");
    let mut app = App::new();
    app.set_config_paths(
        crate::config::AppConfig::default(),
        dir.to_path_buf(),
        dir.to_path_buf(),
    );
    app.handle_command(Command::OpenSettings);
    if let Some(Popup::Settings { tab, draft, .. }) =
        app.state_mut().popup_dialog.active_popup_mut()
    {
        *tab = crate::state::SettingsTab::Appearance;
        draft.appearance_theme_field = crate::state::SettingsField::AppearanceTheme(0);
    }
    let effects = app.handle_key_event(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert!(effects.iter().any(|effect| matches!(
        effect,
        Effect::LoadTheme {
            name,
            purpose: ThemeLoadPurpose::Apply,
            ..
        } if name == "default"
    )));
    assert!(
        effects
            .iter()
            .any(|effect| matches!(effect, Effect::SaveConfig { .. }))
    );
    assert!(matches!(app.active_popup(), Some(Popup::Settings { .. })));
}

#[test]
fn applied_theme_completion_survives_settings_close() {
    let root = unique_temp_dir("settings-theme-close-race");
    let mut app = App::new();
    app.set_config_paths(
        crate::config::AppConfig::default(),
        root.path().to_path_buf(),
        root.path().join("data"),
    );
    let services = AppServices::new().expect("services construction");
    let _ = app.handle_command(Command::OpenSettings);
    if let Some(Popup::Settings { tab, draft, .. }) =
        app.state_mut().popup_dialog.active_popup_mut()
    {
        *tab = SettingsTab::Appearance;
        draft.appearance_theme_field = crate::state::SettingsField::AppearanceTheme(0);
    }
    let effects = app.handle_key_event(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    let load = effects
        .iter()
        .find(|effect| matches!(effect, Effect::LoadTheme { .. }))
        .cloned()
        .expect("applied theme load effect");
    execute_effects(
        vec![load],
        &services,
        &app.state().async_ops.pending_effects,
    );
    app.handle_key_event(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    assert!(app.active_popup_ref().is_none());

    let AppEvent::ThemeLoaded {
        operation_id,
        request_id,
        visit_id,
        themes_dir,
        name,
        theme_names,
        result,
        purpose,
    } = services
        .events()
        .recv_timeout(EVENT_WAIT)
        .expect("applied theme completion")
    else {
        panic!("expected ThemeLoaded completion");
    };
    assert_eq!(purpose, ThemeLoadPurpose::Apply);
    assert!(app.apply_theme_loaded(
        request_id,
        visit_id,
        themes_dir,
        name,
        theme_names,
        result,
        purpose,
    ));
    services.release_operation_event(Some(operation_id));
    services.shutdown();
}

#[test]
fn settings_draft_round_trips_new_fields() {
    let cfg = crate::config::AppConfig {
        keys: crate::config::KeysConfig {
            quit: key("Q"),
            ..Default::default()
        },
        sound: crate::config::SoundConfig {
            output_sink_id: "alsa_output.pci-0000_13_00.6.analog-stereo".to_string(),
        },
        playback: crate::config::PlaybackConfig {
            remote_lyrics: true,
            gain_db: gain(3.0),
            crossfade_seconds: crossfade(15),
        },
        ..Default::default()
    };
    let state = crate::state::AppState {
        confirm_quit: false,
        persistence: crate::state::PersistenceState {
            resume_previous_track: true,
            ..Default::default()
        },
        ..crate::state::AppState::default()
    };
    let draft = crate::state::SettingsDraft::from_state(&state, &cfg, &std::path::PathBuf::new());
    assert!(!draft.confirm_quit);
    assert!(draft.resume_previous_track);
    assert_eq!(draft.keys_draft.quit.as_str(), "Q");
    assert!(
        draft.remote_lyrics,
        "the draft must be seeded from the playback config"
    );
}

#[test]
fn playback_tab_enter_toggles_remote_lyrics_in_the_draft() {
    let mut app = App::new();
    app.handle_command(Command::OpenSettings);
    if let Some(Popup::Settings { tab, .. }) = app.state_mut().popup_dialog.active_popup_mut() {
        *tab = crate::state::SettingsTab::Playback;
    }

    let first = app.handle_key_event(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert!(
        first.is_empty(),
        "the toggle stays in the draft until apply"
    );
    if let Some(Popup::Settings { draft, .. }) = app.active_popup() {
        assert!(
            draft.remote_lyrics,
            "enter must flip the remote lyrics toggle on"
        );
    }

    let second = app.handle_key_event(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert!(second.is_empty());
    if let Some(Popup::Settings { draft, .. }) = app.active_popup() {
        assert!(!draft.remote_lyrics, "enter again must flip it back off");
    }
}

#[test]
fn playback_tab_cursor_moves_rows_and_left_right_steps_gain() {
    let mut app = App::new();
    app.handle_command(Command::OpenSettings);
    if let Some(Popup::Settings { tab, .. }) = app.state_mut().popup_dialog.active_popup_mut() {
        *tab = crate::state::SettingsTab::Playback;
    }

    app.handle_key_event(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
    if let Some(Popup::Settings { draft, .. }) = app.active_popup() {
        assert_eq!(
            draft.playback_field,
            crate::state::SettingsField::PlaybackGain,
            "must move to the gain field"
        );
        assert_eq!(draft.gain_db, crate::audio::GainDb::default());
    }

    app.handle_key_event(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
    if let Some(Popup::Settings { draft, .. }) = app.active_popup() {
        assert!(draft.gain_db == gain(0.5), "right must step the gain up");
    }

    app.handle_key_event(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE));
    if let Some(Popup::Settings { draft, .. }) = app.active_popup() {
        assert!(
            draft.gain_db == crate::audio::GainDb::default(),
            "left must step the gain back down"
        );
    }
}

#[test]
fn playback_tab_gain_steps_saturate_at_both_endpoints() {
    let mut app = App::new();
    app.handle_command(Command::OpenSettings);
    if let Some(Popup::Settings { tab, draft, .. }) =
        app.state_mut().popup_dialog.active_popup_mut()
    {
        *tab = crate::state::SettingsTab::Playback;
        draft.playback_field = crate::state::SettingsField::PlaybackGain;
        draft.gain_db = crate::audio::GainDb::MAX;
    }

    app.handle_key_event(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
    if let Some(Popup::Settings { draft, .. }) = app.active_popup() {
        assert_eq!(draft.gain_db, crate::audio::GainDb::MAX);
    }

    if let Some(Popup::Settings { draft, .. }) = app.state_mut().popup_dialog.active_popup_mut() {
        draft.gain_db = crate::audio::GainDb::MIN;
    }
    app.handle_key_event(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE));
    if let Some(Popup::Settings { draft, .. }) = app.active_popup() {
        assert_eq!(draft.gain_db, crate::audio::GainDb::MIN);
    }
}

#[test]
fn crossfade_slider_clamps_instead_of_wrapping_at_0_and_30() {
    use crate::config::{CROSSFADE_MAX_SECONDS, CROSSFADE_STEP_SECONDS};
    let mut app = App::new();
    app.handle_command(Command::OpenSettings);
    if let Some(Popup::Settings { tab, draft, .. }) =
        app.state_mut().popup_dialog.active_popup_mut()
    {
        *tab = crate::state::SettingsTab::Playback;
        // Land on the crossfade row with the indicator at the max so a
        // forward step must clamp instead of wrapping to Off.
        draft.playback_field = crate::state::SettingsField::PlaybackCrossfade;
        draft.crossfade_seconds = crossfade(CROSSFADE_MAX_SECONDS);
    }

    // Right at the max: another right press must NOT wrap to 0.
    app.handle_key_event(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
    if let Some(Popup::Settings { draft, .. }) = app.active_popup() {
        assert_eq!(
            draft.crossfade_seconds,
            crossfade(CROSSFADE_MAX_SECONDS),
            "right at the max must clamp, not wrap to Off"
        );
    }

    // Seed at Off so a left press must clamp instead of wrapping to max.
    if let Some(Popup::Settings { draft, .. }) = app.state_mut().popup_dialog.active_popup_mut() {
        draft.crossfade_seconds = crossfade(0);
    }

    app.handle_key_event(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE));
    if let Some(Popup::Settings { draft, .. }) = app.active_popup() {
        assert_eq!(
            draft.crossfade_seconds,
            crossfade(0),
            "left at Off must clamp, not wrap to {CROSSFADE_MAX_SECONDS}"
        );
    }

    // Sanity: from a mid value both directions still step by the step.
    if let Some(Popup::Settings { draft, .. }) = app.state_mut().popup_dialog.active_popup_mut() {
        draft.crossfade_seconds = crossfade(CROSSFADE_STEP_SECONDS * 2);
    }
    app.handle_key_event(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
    if let Some(Popup::Settings { draft, .. }) = app.active_popup() {
        assert_eq!(
            draft.crossfade_seconds,
            crossfade(CROSSFADE_STEP_SECONDS * 3),
            "right must still step forward from a mid value"
        );
    }
    app.handle_key_event(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE));
    if let Some(Popup::Settings { draft, .. }) = app.active_popup() {
        assert_eq!(
            draft.crossfade_seconds,
            crossfade(CROSSFADE_STEP_SECONDS * 2),
            "left must still step back from a mid value"
        );
    }
}

#[test]
fn closing_settings_persists_gain_and_emits_setgain_effect() {
    let mut app = App::new();
    let dir = unique_temp_dir("settings-playback-gain");
    app.set_config_paths(
        crate::config::AppConfig::default(),
        dir.to_path_buf(),
        dir.to_path_buf(),
    );
    app.handle_command(Command::OpenSettings);
    if let Some(Popup::Settings { tab, draft, .. }) =
        app.state_mut().popup_dialog.active_popup_mut()
    {
        *tab = crate::state::SettingsTab::Playback;
        draft.gain_db = gain(6.0);
    }

    let effects = app.handle_key_event(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));

    assert!(
        effects.iter().any(|e| matches!(
            e,
            Effect::Audio(crate::audio::AudioCommand::SetGain(g))
                if *g == gain(6.0)
        )),
        "closing with a gain change must emit SetGain"
    );
    assert!(
        app.config.playback.gain_db == gain(6.0),
        "the gain preference must stick in the config"
    );
}

#[test]
fn closing_settings_persists_remote_lyrics_and_emits_the_effect() {
    let mut app = App::new();
    let dir = unique_temp_dir("settings-remote-lyrics");
    app.set_config_paths(
        crate::config::AppConfig::default(),
        dir.to_path_buf(),
        dir.to_path_buf(),
    );
    app.handle_command(Command::OpenSettings);
    if let Some(Popup::Settings { tab, draft, .. }) =
        app.state_mut().popup_dialog.active_popup_mut()
    {
        *tab = crate::state::SettingsTab::Playback;
        draft.remote_lyrics = true;
    }

    let effects = app.handle_key_event(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));

    assert!(
        effects
            .iter()
            .any(|e| matches!(e, Effect::SetLyricsRemote(true))),
        "closing with a change must emit the runtime effect"
    );
    assert!(
        app.config.playback.remote_lyrics,
        "the preference must stick in the config"
    );

    // Re-opening seeds the draft from the persisted config.
    app.handle_command(Command::OpenSettings);
    if let Some(Popup::Settings { draft, .. }) = app.active_popup() {
        assert!(
            draft.remote_lyrics,
            "the draft must mirror the persisted value"
        );
    }
}

#[test]
fn opening_settings_uses_a_placeholder_without_blocking_keys_or_rendering() {
    let mut app = App::new();
    app.set_output_provider(Arc::new(DelayedOutputProvider {
        delay: Duration::from_millis(300),
        outputs: vec![AudioOutput {
            id: "slow-sink".to_string(),
            name: "Slow sink".to_string(),
            description: String::new(),
            is_default: false,
            available: true,
            node_id: Some(7),
        }],
    }));

    let started = std::time::Instant::now();
    let effects = app.handle_command(Command::OpenSettings);
    assert!(
        started.elapsed() < Duration::from_millis(150),
        "opening Settings must not enumerate on the UI thread: {:?}",
        started.elapsed()
    );
    assert!(
        effects
            .iter()
            .any(|effect| matches!(effect, Effect::EnumerateOutputs { .. }))
    );
    assert!(
        effects
            .iter()
            .any(|effect| matches!(effect, Effect::LoadTheme { .. }))
    );
    assert!(matches!(
        app.active_popup(),
        Some(Popup::Settings { draft, .. })
            if draft.outputs.len() == 1 && draft.outputs[0].id.is_empty()
    ));

    if let Some(Popup::Settings { tab, .. }) = app.state_mut().popup_dialog.active_popup_mut() {
        *tab = SettingsTab::Sound;
    }
    let mut terminal =
        ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 24)).expect("test terminal");
    let started = std::time::Instant::now();
    terminal
        .draw(|frame| crate::ui::render(frame, &app, &crate::ui::theme::Theme::default()))
        .expect("settings render");
    assert!(
        started.elapsed() < Duration::from_millis(150),
        "rendering the placeholder must not wait for enumeration: {:?}",
        started.elapsed()
    );
}

#[test]
fn stale_output_enumeration_cannot_replace_a_newer_settings_visit() {
    let (started_tx, started_rx) = std::sync::mpsc::channel();
    let provider = Arc::new(SequencedOutputProvider {
        started: Mutex::new(Some(started_tx)),
        release_first: Arc::new(Barrier::new(2)),
        calls: AtomicUsize::new(0),
        first: vec![AudioOutput {
            id: "old-sink".to_string(),
            name: "Old sink".to_string(),
            description: String::new(),
            is_default: false,
            available: true,
            node_id: Some(1),
        }],
        second: vec![AudioOutput {
            id: "new-sink".to_string(),
            name: "New sink".to_string(),
            description: String::new(),
            is_default: false,
            available: true,
            node_id: Some(2),
        }],
    });
    let release_first = Arc::clone(&provider.release_first);
    let mut app = App::new();
    app.set_output_provider(provider);
    let services = AppServices::new().expect("services construction");

    let first_effects = app.handle_command(Command::OpenSettings);
    execute_effects(
        first_effects,
        &services,
        &app.state().async_ops.pending_effects,
    );
    started_rx
        .recv_timeout(EVENT_WAIT)
        .expect("first output enumeration started");

    app.handle_key_event(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    let second_effects = app.handle_command(Command::OpenSettings);
    execute_effects(
        second_effects,
        &services,
        &app.state().async_ops.pending_effects,
    );

    let (operation_id, request_id, outputs) = loop {
        let event = services
            .events()
            .recv_timeout(EVENT_WAIT)
            .expect("new output enumeration result");
        match event {
            AppEvent::OutputsEnumerated {
                operation_id,
                request_id,
                outputs,
            } => break (operation_id, request_id, outputs),
            event => {
                let operation_id = match &event {
                    AppEvent::ThemeLoaded { operation_id, .. } => Some(*operation_id),
                    _ => None,
                };
                services.release_operation_event(operation_id);
            }
        }
    };
    assert!(services.accepts_operation_completion(operation_id));
    assert_eq!(outputs[0].id, "new-sink");
    assert!(app.apply_outputs_enumerated(request_id, outputs));
    assert!(matches!(
        app.active_popup(),
        Some(Popup::Settings { draft, .. })
            if draft.outputs.iter().any(|output| output.id == "new-sink")
    ));
    services.release_operation_event(Some(operation_id));

    release_first.wait();
    while let Ok(event) = services.events().recv_timeout(Duration::from_millis(100)) {
        if let AppEvent::OutputsEnumerated { outputs, .. } = &event {
            assert_ne!(
                outputs.first().map(|output| output.id.as_str()),
                Some("old-sink"),
                "the superseded enumeration must not publish its stale result"
            );
        }
        let operation_id = match &event {
            AppEvent::ThemeLoaded { operation_id, .. }
            | AppEvent::OutputsEnumerated { operation_id, .. } => Some(*operation_id),
            _ => None,
        };
        services.release_operation_event(operation_id);
    }
    services.shutdown();
}

#[test]
fn sound_output_selection_is_applied_and_persisted() {
    use crate::audio::{AudioOutput, StaticOutputProvider};
    use std::sync::Arc;

    let mut app = App::new();
    let dir = unique_temp_dir("settings-output");
    app.set_config_paths(
        crate::config::AppConfig::default(),
        dir.to_path_buf(),
        dir.to_path_buf(),
    );
    let outputs = vec![
        AudioOutput::session_default(),
        AudioOutput {
            id: "sink-a".to_string(),
            name: "Sink A".to_string(),
            description: String::new(),
            is_default: false,
            available: true,
            node_id: Some(57),
        },
    ];
    app.set_output_provider(Arc::new(StaticOutputProvider::new(outputs, None)));
    app.handle_command(Command::OpenSettings);
    if let Some(Popup::Settings { tab, draft, .. }) =
        app.state_mut().popup_dialog.active_popup_mut()
    {
        *tab = crate::state::SettingsTab::Sound;
        draft.sound_field = crate::state::SettingsField::SoundOutput(1);
    }
    let effects = app.handle_key_event(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert!(effects.iter().any(|effect| matches!(
        effect,
        Effect::Audio(AudioCommand::SetOutput(target))
            if target.node_id == Some(57) && target.stable_id.as_deref() == Some("sink-a")
    )));
    assert!(
        app.state()
            .notifications
            .iter()
            .any(|n| n.contains("Output set to Sink A")),
        "selection must notify"
    );
    let services = AppServices::new().expect("services construction");
    execute_effects(effects, &services, &app.state().async_ops.pending_effects);
    let AppEvent::ConfigSaved { operation_id, .. } = services
        .events()
        .recv_timeout(EVENT_WAIT)
        .expect("config save completion")
    else {
        panic!("expected config save completion");
    };
    services.release_operation_event(Some(operation_id));
    let contents = std::fs::read_to_string(dir.join("config.toml")).expect("config written");
    assert!(
        contents.contains("sink-a"),
        "stable sink id must be persisted: {contents}"
    );
    services.shutdown();
}

#[test]
fn sound_output_selection_round_trips_default_concrete_default() {
    use crate::audio::{AudioCommand, AudioOutput, StaticOutputProvider};
    use std::sync::Arc;

    let mut app = App::new();
    let outputs = vec![
        AudioOutput::session_default(),
        AudioOutput {
            id: "sink-a".to_string(),
            name: "Sink A".to_string(),
            description: String::new(),
            is_default: false,
            available: true,
            node_id: Some(57),
        },
    ];
    app.set_output_provider(Arc::new(StaticOutputProvider::new(outputs, None)));

    app.handle_command(Command::OpenSettings);
    if let Some(Popup::Settings { tab, draft, .. }) =
        app.state_mut().popup_dialog.active_popup_mut()
    {
        *tab = crate::state::SettingsTab::Sound;
        draft.sound_field = crate::state::SettingsField::SoundOutput(1);
    }
    let to_concrete = app.handle_key_event(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    assert_eq!(app.config.sound.output_sink_id, "sink-a");
    assert!(to_concrete.iter().any(|effect| matches!(
        effect,
        Effect::Audio(AudioCommand::SetOutput(target))
            if target.node_id == Some(57) && target.stable_id.as_deref() == Some("sink-a")
    )));

    app.handle_command(Command::OpenSettings);
    if let Some(Popup::Settings { tab, draft, .. }) =
        app.state_mut().popup_dialog.active_popup_mut()
    {
        *tab = crate::state::SettingsTab::Sound;
        draft.sound_field = crate::state::SettingsField::SoundOutput(0);
    }
    let to_default = app.handle_key_event(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    assert_eq!(app.config.sound.output_sink_id, "");
    assert!(to_default.iter().any(|effect| matches!(
        effect,
        Effect::Audio(AudioCommand::SetOutput(target))
            if target.node_id.is_none() && target.stable_id.is_none()
    )));
}

#[test]
fn closing_settings_without_a_sound_change_does_not_rebuild_the_output() {
    use crate::audio::{AudioCommand, AudioOutput, StaticOutputProvider};
    use std::sync::Arc;

    let mut app = App::new();
    let dir = unique_temp_dir("settings-no-output-change");
    app.set_config_paths(
        crate::config::AppConfig::default(),
        dir.to_path_buf(),
        dir.to_path_buf(),
    );
    // Pre-configure the same sink so the draft matches the persisted choice.
    app.config.sound.output_sink_id = "sink-a".to_string();
    let outputs = vec![
        AudioOutput::session_default(),
        AudioOutput {
            id: "sink-a".to_string(),
            name: "Sink A".to_string(),
            description: String::new(),
            is_default: false,
            available: true,
            node_id: Some(57),
        },
    ];
    app.set_output_provider(Arc::new(StaticOutputProvider::new(outputs, None)));
    app.handle_command(Command::OpenSettings);
    if let Some(Popup::Settings { tab, draft, .. }) =
        app.state_mut().popup_dialog.active_popup_mut()
    {
        *tab = crate::state::SettingsTab::Sound;
        draft.sound_field = crate::state::SettingsField::SoundOutput(1); // same "Sink A" as persisted
    }

    // Esc applies settings; since the selected device is unchanged, the
    // worker must NOT receive a redundant SetOutput that would tear down
    // and reopen the PipeWire stream (the momentary playback pause).
    let effects = app.handle_key_event(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    assert!(
        !effects
            .iter()
            .any(|e| matches!(e, Effect::Audio(AudioCommand::SetOutput(_)))),
        "closing without changing the device must not rebuild the output stream"
    );
}

#[test]
fn closing_settings_after_a_sound_change_does_rebuild_the_output() {
    use crate::audio::{AudioCommand, AudioOutput, StaticOutputProvider};
    use std::sync::Arc;

    let mut app = App::new();
    let dir = unique_temp_dir("settings-output-change");
    app.set_config_paths(
        crate::config::AppConfig::default(),
        dir.to_path_buf(),
        dir.to_path_buf(),
    );
    app.config.sound.output_sink_id = "".to_string(); // currently on default
    let outputs = vec![
        AudioOutput::session_default(),
        AudioOutput {
            id: "sink-a".to_string(),
            name: "Sink A".to_string(),
            description: String::new(),
            is_default: false,
            available: true,
            node_id: Some(57),
        },
    ];
    app.set_output_provider(Arc::new(StaticOutputProvider::new(outputs, None)));
    app.handle_command(Command::OpenSettings);
    if let Some(Popup::Settings { tab, draft, .. }) =
        app.state_mut().popup_dialog.active_popup_mut()
    {
        *tab = crate::state::SettingsTab::Sound;
        draft.sound_field = crate::state::SettingsField::SoundOutput(1); // switch from default to "Sink A"
    }

    let effects = app.handle_key_event(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    assert!(
        effects.iter().any(|e| matches!(
            e,
            Effect::Audio(AudioCommand::SetOutput(target))
                if target.node_id == Some(57)
        )),
        "changing the device must rebuild the output stream once"
    );
}

#[test]
fn disappearing_output_is_rejected_by_the_live_provider_contract() {
    use crate::audio::{AudioCommand, AudioOutput};
    use std::sync::Arc;

    let mut app = App::new();
    let dir = unique_temp_dir("settings-disappearing-output");
    app.set_config_paths(
        AppConfig::default(),
        dir.path().join("config"),
        dir.path().join("data"),
    );
    app.set_output_provider(Arc::new(DisappearingOutputProvider {
        output: AudioOutput {
            id: "vanished-sink".to_string(),
            name: "Vanished sink".to_string(),
            description: String::new(),
            is_default: false,
            available: true,
            node_id: Some(99),
        },
    }));
    app.handle_command(Command::OpenSettings);
    if let Some(Popup::Settings { tab, draft, .. }) =
        app.state_mut().popup_dialog.active_popup_mut()
    {
        *tab = SettingsTab::Sound;
        draft.sound_field = crate::state::SettingsField::SoundOutput(1);
    }

    let effects = app.handle_key_event(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));

    assert!(
        !effects
            .iter()
            .any(|effect| matches!(effect, Effect::Audio(AudioCommand::SetOutput(_))))
    );
    assert_eq!(app.config.sound.output_sink_id, "");
    assert!(
        app.state()
            .popup_dialog
            .alert_message()
            .is_some_and(|message| message.contains("vanished-sink"))
    );
    let config = AppConfig::load(&dir.path().join("config"));
    assert_ne!(config.sound.output_sink_id, "vanished-sink");
}

#[test]
fn unavailable_saved_output_stays_visible_and_persisted_when_settings_close() {
    use crate::audio::{AudioOutput, StaticOutputProvider};
    use std::sync::Arc;

    let mut app = App::new();
    let dir = unique_temp_dir("settings-unavailable-output");
    let mut config = crate::config::AppConfig::default();
    config.sound.output_sink_id = "disconnected-sink".to_string();
    app.set_config_paths(config, dir.to_path_buf(), dir.to_path_buf());
    app.set_output_provider(Arc::new(StaticOutputProvider::new(
        vec![AudioOutput::session_default()],
        None,
    )));

    app.handle_command(Command::OpenSettings);
    let (selected_output, output_count) = match app.active_popup() {
        Some(Popup::Settings { draft, .. }) => (
            draft.outputs[draft.selected_output().expect("output selection")].clone(),
            draft.outputs.len(),
        ),
        _ => panic!("settings popup must be open"),
    };
    assert_eq!(selected_output.id, "disconnected-sink");
    assert!(!selected_output.available);
    assert_eq!(output_count, 2);

    let effects = app.handle_key_event(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));

    assert!(
        effects
            .iter()
            .any(|effect| matches!(effect, Effect::SaveConfig { .. }))
    );
    assert_eq!(app.config.sound.output_sink_id, "disconnected-sink");
}

#[test]
fn sort_action_requires_confirmation_and_preserves_settings_state() {
    use crate::config::{PlaylistColumnsConfig, SortBy};

    let mut app = playing_fixture(0);
    app.state.active_playlist_name = Some("Sorted".to_string());
    // Give paths distinct file names so the sort has a definite order.
    app.state.playlist = {
        let mut pl = crate::playlist::Playlist::new();
        pl.extend([
            crate::track::Track::local(PathBuf::from("/music/zz.mp3")),
            crate::track::Track::local(PathBuf::from("/music/aa.mp3")),
            crate::track::Track::local(PathBuf::from("/music/mm.mp3")),
        ]);
        pl
    };

    app.handle_command(Command::OpenSettings);
    if let Some(Popup::Settings { draft, .. }) = app.state_mut().popup_dialog.active_popup_mut() {
        // The presentation choice supplies the criteria for the explicit
        // action but does not itself reorder the queue.
        draft.playlist_columns = PlaylistColumnsConfig {
            display_by: SortBy::Metadata,
            ..PlaylistColumnsConfig::default()
        };
        draft.general_field = crate::state::SettingsField::GeneralSortTracks;
    }

    app.handle_key_event(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert!(matches!(
        app.active_popup(),
        Some(Popup::ConfirmSortTracks { .. })
    ));
    let before_cancel: Vec<String> = app
        .state()
        .playlist
        .tracks()
        .iter()
        .map(|t| t.display_name().into_owned())
        .collect();
    app.handle_key_event(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    assert!(matches!(app.active_popup(), Some(Popup::Settings { .. })));
    assert_eq!(
        app.state()
            .playlist
            .tracks()
            .iter()
            .map(|t| t.display_name().into_owned())
            .collect::<Vec<_>>(),
        before_cancel,
        "cancelling sort must leave the queue untouched"
    );

    app.handle_key_event(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    let effects = app.handle_key_event(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert!(matches!(
        effects.as_slice(),
        [Effect::SaveActivePlaylist { name, .. }] if name == "Sorted"
    ));

    let names: Vec<String> = app
        .state()
        .playlist
        .tracks()
        .iter()
        .map(|t| t.display_name().into_owned())
        .collect();
    assert_eq!(
        names,
        vec!["aa", "mm", "zz"],
        "explicit confirmation reorders the playlist"
    );
    app.handle_key_event(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    assert!(app.active_popup().is_none());
}

#[test]
fn closing_settings_without_sort_change_leaves_order_alone() {
    let mut app = playing_fixture(0);
    app.handle_command(Command::OpenSettings);
    if let Some(Popup::Settings { draft, .. }) = app.state_mut().popup_dialog.active_popup_mut() {
        // Keep the default (Filename) selection unchanged.
        draft.general_field = crate::state::SettingsField::GeneralSortTracks;
    }

    app.handle_key_event(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));

    let names: Vec<String> = app
        .state()
        .playlist
        .tracks()
        .iter()
        .map(|t| t.display_name().into_owned())
        .collect();
    assert_eq!(
        names,
        vec!["a", "b", "c"],
        "no reorder when nothing changed"
    );
}

#[test]
fn sub_option_edits_under_filename_do_not_reorder_on_settings_close() {
    use crate::config::{PlaylistColumnsConfig, SortBy};

    // Playlist in a deliberately non-alphabetical order to detect a reorder.
    let mut app = playing_fixture(0);
    app.state.playlist = {
        let mut pl = crate::playlist::Playlist::new();
        pl.extend([
            crate::track::Track::local(PathBuf::from("/music/zz.mp3")),
            crate::track::Track::local(PathBuf::from("/music/aa.mp3")),
            crate::track::Track::local(PathBuf::from("/music/mm.mp3")),
        ]);
        pl
    };

    app.handle_command(Command::OpenSettings);
    if let Some(Popup::Settings { draft, .. }) = app.state_mut().popup_dialog.active_popup_mut() {
        // The user toggles Metadata and some sub-options, then returns to
        // Filename before closing. The final config (Filename) is
        // effectively the same as the initial one, so no reorder may run.
        draft.playlist_columns = PlaylistColumnsConfig {
            display_by: SortBy::Metadata,
            metadata_artist: true,
            ..PlaylistColumnsConfig::default()
        };
        draft.playlist_columns = PlaylistColumnsConfig {
            display_by: SortBy::Filename, // back to Filename
            metadata_artist: true,
            ..PlaylistColumnsConfig::default()
        };
    }

    app.handle_key_event(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));

    let names: Vec<String> = app
        .state()
        .playlist
        .tracks()
        .iter()
        .map(|t| t.display_name().into_owned())
        .collect();
    assert_eq!(
        names,
        vec!["zz", "aa", "mm"],
        "closing Settings must not reorder even after toggling metadata in between"
    );
}

#[test]
fn explicit_sort_remaps_the_playing_track_index_to_its_new_position() {
    use crate::config::{PlaylistColumnsConfig, SortBy};

    // A playlist whose order changes under a Filename sort, with track /c
    // (index 2) currently playing.
    let mut app = playing_fixture(0);
    app.state.playlist = {
        let mut pl = crate::playlist::Playlist::new();
        pl.extend([
            crate::track::Track::local(PathBuf::from("/music/zz.mp3")),
            crate::track::Track::local(PathBuf::from("/music/mm.mp3")),
            crate::track::Track::local(PathBuf::from("/music/aa.mp3")),
        ]);
        pl
    };
    app.state.playlist.select(2); // select /aa (after sort it becomes index 0)
    app.state.playback.track_index = Some(2);
    let playing_location = app.state.playlist.current().unwrap().track_location();

    let columns = PlaylistColumnsConfig {
        display_by: SortBy::Filename,
        ..PlaylistColumnsConfig::default()
    };
    let effects = app.apply_sort_tracks(&columns);
    assert!(effects.is_empty());

    // The playing track must still be the same song, at its NEW index.
    let new_index = app.state.playback.track_index.expect("track still playing");
    let still_playing = app
        .state()
        .playlist
        .tracks()
        .get(new_index)
        .map(Track::track_location);
    assert_eq!(
        still_playing,
        Some(playing_location),
        "Now Playing must keep pointing at the song that is actually playing after a sort"
    );
    assert_eq!(
        app.state().playlist.cursor(),
        new_index,
        "the selection cursor follows the playing track"
    );
}

#[test]
fn playlist_column_sub_options_are_immutable_while_filename_is_selected() {
    use crate::config::{PlaylistColumnsConfig, SortBy};
    use crate::state::{Popup, SettingsTab};

    let mut app = App::new();
    app.handle_command(Command::OpenSettings);
    // Metadata off (Filename) with Artist already stored.
    if let Some(Popup::Settings { tab, draft, .. }) =
        app.state_mut().popup_dialog.active_popup_mut()
    {
        *tab = SettingsTab::Appearance;
        draft.appearance_column = crate::state::AppearanceColumn::Display;
        draft.appearance_display_field = crate::state::AppearanceDisplayField::PlaylistMetadata(
            crate::config::SortMetadataField::Artist,
        );
        draft.playlist_columns = PlaylistColumnsConfig {
            display_by: SortBy::Filename,
            metadata_artist: true,
            ..PlaylistColumnsConfig::default()
        };
    }

    // Pressing Enter on the Artist sub-option while Metadata is off must
    // NOT toggle it.
    app.handle_key_event(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

    let on = app
        .state()
        .popup_dialog
        .active_popup_ref()
        .and_then(|p| match p {
            Popup::Settings { draft, .. } => Some(draft.playlist_columns.metadata_artist),
            _ => None,
        })
        .unwrap_or(false);
    assert!(on, "Artist stays in its stored state when Metadata is off");
}

#[test]
fn playlist_column_cursors_toggle_the_canonical_fields() {
    use crate::config::{PlaylistColumnsConfig, SortBy};
    use crate::state::{Popup, SettingsTab};

    let mut app = App::new();
    app.handle_command(Command::OpenSettings);
    if let Some(Popup::Settings { tab, draft, .. }) =
        app.state_mut().popup_dialog.active_popup_mut()
    {
        *tab = SettingsTab::Appearance;
        draft.appearance_column = crate::state::AppearanceColumn::Display;
        draft.appearance_display_field = crate::state::AppearanceDisplayField::PlaylistMetadata(
            crate::config::SortMetadataField::Artist,
        );
        draft.playlist_columns = PlaylistColumnsConfig {
            display_by: SortBy::Metadata,
            ..PlaylistColumnsConfig::default()
        };
    }

    for field in [
        crate::config::SortMetadataField::Artist,
        crate::config::SortMetadataField::Album,
        crate::config::SortMetadataField::TrackNumber,
    ] {
        if let Some(Popup::Settings { draft, .. }) = app.state_mut().popup_dialog.active_popup_mut()
        {
            draft.appearance_display_field =
                crate::state::AppearanceDisplayField::PlaylistMetadata(field);
        }
        app.handle_key_event(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        let flags = match app.state().popup_dialog.active_popup_ref() {
            Some(Popup::Settings { draft, .. }) => (
                draft.playlist_columns.metadata_artist,
                draft.playlist_columns.metadata_album,
                draft.playlist_columns.metadata_track_number,
            ),
            _ => panic!("settings popup must remain open while toggling metadata"),
        };
        let expected = match field {
            crate::config::SortMetadataField::Artist => (true, false, false),
            crate::config::SortMetadataField::Album => (true, true, false),
            crate::config::SortMetadataField::TrackNumber => (true, true, true),
            crate::config::SortMetadataField::Title => unreachable!(),
        };
        assert_eq!(
            flags, expected,
            "metadata field {field:?} mapped incorrectly"
        );
    }
}

#[test]
fn stale_snapshot_index_is_remapped_to_the_playing_track() {
    use crate::audio::PlaybackSnapshot;

    // A queue already in its post-sort order; /aa is now at index 0.
    let mut app = playing_fixture(0);
    app.state.playlist = {
        let mut pl = crate::playlist::Playlist::new();
        pl.extend([
            crate::track::Track::local(PathBuf::from("/music/aa.mp3")),
            crate::track::Track::local(PathBuf::from("/music/mm.mp3")),
            crate::track::Track::local(PathBuf::from("/music/zz.mp3")),
        ]);
        pl
    };
    app.state.persistence.last_track = Some(TrackLocation::local("/music/aa.mp3"));
    app.state.playback.track_index = Some(0);

    // Simulate the worker reporting the PRE-sort index (2) for a track that
    // is now at index 0 after a sort: the snapshot must be re-mapped.
    let stale = PlaybackSnapshot {
        status: crate::audio::PlayStatus::Playing,
        track_index: Some(2),
        elapsed: std::time::Duration::ZERO,
        duration: None,
        sink_health: crate::audio::SinkHealth::Healthy,
    };
    app.apply_playback_progress(stale);
    assert_eq!(
        app.state.playback.track_index,
        Some(0),
        "stale index remapped by stable location"
    );

    // A current index (0 on /aa) stays as-is.
    let current = PlaybackSnapshot {
        status: crate::audio::PlayStatus::Playing,
        track_index: Some(0),
        elapsed: std::time::Duration::ZERO,
        duration: None,
        sink_health: crate::audio::SinkHealth::Healthy,
    };
    app.apply_playback_progress(current);
    assert_eq!(
        app.state.playback.track_index,
        Some(0),
        "current index is kept"
    );
}

#[test]
fn keyboard_sort_action_reorders_after_confirmation() {
    // Non-alphabetical queue reordered only after the explicit action is
    // confirmed, never merely by closing Settings.
    let mut app = playing_fixture(0);
    app.state.playlist = {
        let mut pl = crate::playlist::Playlist::new();
        pl.extend([
            crate::track::Track::local(PathBuf::from("/music/zz.mp3")),
            crate::track::Track::local(PathBuf::from("/music/mm.mp3")),
            crate::track::Track::local(PathBuf::from("/music/aa.mp3")),
        ]);
        pl
    };

    app.handle_command(Command::OpenSettings);
    app.state.active_playlist_name = Some("Sorted".to_string());
    if let Some(Popup::Settings { draft, .. }) = app.state_mut().popup_dialog.active_popup_mut() {
        draft.playlist_columns.display_by = crate::config::SortBy::Metadata;
        draft.general_field = crate::state::SettingsField::GeneralSortTracks;
    }
    app.handle_key_event(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert!(matches!(
        app.active_popup(),
        Some(Popup::ConfirmSortTracks { .. })
    ));
    let before_cancel = app
        .state()
        .playlist
        .tracks()
        .iter()
        .map(|t| t.display_name().into_owned())
        .collect::<Vec<_>>();
    app.handle_key_event(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    assert_eq!(
        app.state()
            .playlist
            .tracks()
            .iter()
            .map(|t| t.display_name().into_owned())
            .collect::<Vec<_>>(),
        before_cancel
    );
    app.handle_key_event(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    let effects = app.handle_key_event(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert!(matches!(
        effects.as_slice(),
        [Effect::SaveActivePlaylist { name, .. }] if name == "Sorted"
    ));
    app.handle_key_event(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));

    let names: Vec<String> = app
        .state()
        .playlist
        .tracks()
        .iter()
        .map(|t| t.display_name().into_owned())
        .collect();
    assert_eq!(
        names,
        vec!["aa", "mm", "zz"],
        "changing to Metadata via the confirmed action must reorder"
    );
}

#[test]
fn track_ended_after_a_reorder_advances_to_the_next_in_the_new_order() {
    // Playlist already in its POST-sort Filename order: aa(0), mm(1), zz(2).
    // The worker still tags aa with its PRE-sort index (2), because it
    // only knows the path and started before the sort ran.
    let mut app = playing_fixture(0);
    app.state.playlist = {
        let mut pl = crate::playlist::Playlist::new();
        pl.extend([
            crate::track::Track::local(PathBuf::from("/music/aa.mp3")),
            crate::track::Track::local(PathBuf::from("/music/mm.mp3")),
            crate::track::Track::local(PathBuf::from("/music/zz.mp3")),
        ]);
        pl
    };
    // aa is playing at its current index (0); the worker later reports 2.
    app.state.persistence.last_track = Some(TrackLocation::local("/music/aa.mp3"));
    app.state.playback.track_index = Some(0);
    app.state.playlist.select(0);

    // The worker reports the STALE pre-sort index 2 when aa ends; the app
    // must advance from aa's CURRENT position instead.
    let effects = app.apply_track_ended(2);

    // Next track after aa in the new order: mm (index 1).
    let play = effects
        .iter()
        .find_map(|e| match e {
            Effect::Audio(AudioCommand::Play { source, .. }) => Some(source.clone()),
            _ => None,
        })
        .expect("next track must start playing");
    assert_eq!(
        play.display_location(),
        "/music/mm.mp3",
        "the queue must continue with the next track in the reordered playlist"
    );
    assert_eq!(
        app.state().playback.track_index,
        Some(1),
        "playback points at the next track in the new order"
    );
}

#[test]
fn toggle_lyrics_shows_the_panel_and_requests_a_load() {
    let mut app = playing_fixture(1);
    app.state.artwork.set_enabled(true);

    let effects = app.handle_command(Command::ToggleLyrics);

    assert!(app.state().lyrics.visible);
    assert_eq!(app.active_panel(), Panel::Lyrics);
    // The load request carries the playing queue entry
    assert!(
        effects
            .iter()
            .any(|e| matches!(e, Effect::LoadLyrics { track_index: 1, .. }))
    );
    assert!(
        !effects
            .iter()
            .any(|e| matches!(e, Effect::LoadArtwork { .. })),
        "toggling the panel only requests lyrics; artwork loads on track start"
    );
    assert!(app.state().lyrics.loading);
}

#[test]
fn toggle_lyrics_hides_the_panel_without_reloading_artwork() {
    let mut app = playing_fixture(1);
    app.state.artwork.set_enabled(true);
    app.handle_command(Command::ToggleLyrics);
    assert!(app.state().lyrics.visible);

    let effects = app.handle_command(Command::ToggleLyrics);

    assert!(!app.state().lyrics.visible);
    assert_eq!(
        app.active_panel(),
        Panel::Browser,
        "focus returns to the browser"
    );
    assert!(
        !effects
            .iter()
            .any(|e| matches!(e, Effect::LoadArtwork { .. })),
        "hiding the panel never reloads artwork: the overlay retargets itself"
    );
}

#[test]
fn escape_hides_the_lyrics_panel_only_when_focused() {
    let mut app = playing_fixture(0);
    app.handle_command(Command::ToggleLyrics);
    let effects = app.handle_command(Command::CancelPopup);
    assert!(!app.state().lyrics.visible);
    assert!(
        !effects
            .iter()
            .any(|e| matches!(e, Effect::LoadLyrics { .. })),
        "escaping only hides, it never spawns a load"
    );

    // A popup still takes priority: esc closes it and nothing else moves.
    app.handle_command(Command::Quit);
    app.state.popup_dialog.clear();
    app.handle_command(Command::ToggleHelp);
    assert!(app.active_popup().is_some());
    app.handle_command(Command::CancelPopup);
    assert!(app.active_popup().is_none());
}

#[test]
fn load_metadata_for_queue_requests_local_tracks_with_provisional_metadata() {
    use crate::playlist::Playlist;
    use crate::stream::StreamKind;
    use crate::track::Track;

    let mut app = App::new();
    let provisional = |title: &str| TrackMetadata {
        title: title.to_string(),
        title_tagged: true,
        ..TrackMetadata::default()
    };
    let mut first = Track::local("/a.mp3");
    first.set_metadata(provisional("A"));
    let mut second = Track::local("/b.mp3");
    second.set_metadata(provisional("B"));
    let stream = Track::from_stream(
        url::Url::parse("https://radio.example.com/live").expect("valid URL"),
        StreamKind::Http,
    );
    app.state.playlist = {
        let mut playlist = Playlist::new();
        playlist.extend([first, second, stream]);
        playlist
    };

    let effects = app.load_metadata_for_queue();

    match effects.as_slice() {
        [Effect::LoadMetadata(paths)] => {
            assert_eq!(paths.len(), 2, "only local tracks must be requested");
            assert!(paths.contains(&PathBuf::from("/a.mp3")));
            assert!(paths.contains(&PathBuf::from("/b.mp3")));
            assert!(
                !paths
                    .iter()
                    .any(|path| path.to_string_lossy().contains("radio")),
                "streams must remain excluded"
            );
        }
        other => panic!("expected one LoadMetadata effect, got {other:?}"),
    }
}

#[test]
fn load_metadata_for_queue_is_a_no_op_for_an_empty_queue() {
    let app = App::new();
    assert!(app.load_metadata_for_queue().is_empty());
}

#[test]
fn play_start_with_lyrics_visible_loads_artwork_and_lyrics() {
    let mut app = App::new();
    app.state
        .extend_playlist(["/x.flac", "/y.mp3"].map(PathBuf::from));
    app.state.playlist.select(1);
    app.state.artwork.set_enabled(true);
    app.state.lyrics.visible = true;

    let effects = app.handle_command(Command::PlaySelected);

    assert!(
        effects
            .iter()
            .any(|e| matches!(e, Effect::LoadArtwork { .. })),
        "artwork still loads with lyrics visible, its overlay retargets the playlist"
    );
    assert!(
        effects
            .iter()
            .any(|e| matches!(e, Effect::LoadLyrics { track_index: 1, .. }))
    );
}

#[test]
fn apply_lyrics_loaded_merges_the_document_for_the_playing_track() {
    let mut app = playing_fixture(1);
    app.handle_command(Command::ToggleLyrics);

    let outcome = crate::lyrics::LoadOutcome {
        document: Some(crate::lyrics::LyricsDocument::from_plain(
            "line one\nline two",
        )),
        origin: Some(crate::lyrics::LyricsOrigin::Local),
        saved_path: None,
        error: None,
    };
    app.apply_lyrics_loaded(1, outcome);

    assert!(!app.state().lyrics.loading);
    assert_eq!(
        app.state().lyrics.origin,
        Some(crate::lyrics::LyricsOrigin::Local)
    );
    assert_eq!(
        app.state()
            .lyrics
            .document
            .as_ref()
            .map(|d| d.text.as_str()),
        Some("line one\nline two")
    );
}

#[test]
fn apply_lyrics_loaded_drops_stale_resolutions() {
    let mut app = playing_fixture(1);
    app.state.lyrics.visible = true;
    app.state.lyrics.loading = true;

    let outcome = crate::lyrics::LoadOutcome {
        document: Some(crate::lyrics::LyricsDocument::from_plain("stale")),
        origin: None,
        saved_path: None,
        error: None,
    };
    app.apply_lyrics_loaded(0, outcome);

    assert!(
        app.state().lyrics.loading,
        "a late delivery for a track the user left must not clear state"
    );
}

#[test]
fn lyrics_scroll_commands_clamp_within_the_document() {
    let mut app = App::new();
    app.state.lyrics.document = Some(crate::lyrics::LyricsDocument::from_plain(
        &(0..30)
            .map(|i| format!("row {i}"))
            .collect::<Vec<_>>()
            .join("\n"),
    ));
    app.state.lyrics.viewport_height = 8;
    app.state.active_panel = Panel::Lyrics;
    app.state
        .tick_frame(FrameMetrics::new(1, 1, 8, 100), Duration::ZERO);

    app.handle_command(Command::CursorDown);
    assert_eq!(app.state().lyrics.scroll, 1);
    app.handle_command(Command::PageDown);
    assert_eq!(
        app.state().lyrics.scroll,
        1 + 6,
        "page step is viewport minus two"
    );
    app.handle_command(Command::CursorBottom);
    assert_eq!(
        app.state().lyrics.scroll,
        22,
        "bottom clamps to lines - viewport"
    );
    app.handle_command(Command::CursorDown);
    assert_eq!(app.state().lyrics.scroll, 22, "past the end stays clamped");
    app.handle_command(Command::CursorTop);
    assert_eq!(app.state().lyrics.scroll, 0);
    app.handle_command(Command::CursorUp);
    assert_eq!(app.state().lyrics.scroll, 0, "up from the top is a no-op");
    app.handle_command(Command::PageUp);
    assert_eq!(app.state().lyrics.scroll, 0);
}

#[test]
fn lyrics_scroll_is_a_safe_no_op_without_a_document() {
    let mut app = App::new();
    app.state.active_panel = Panel::Lyrics;
    app.handle_command(Command::CursorDown);
    assert_eq!(app.state().lyrics.scroll, 0);
}

#[test]
fn lyrics_scroll_clamps_against_cached_physical_rows() {
    let mut app = App::new();
    app.state
        .lyrics
        .set_document(Some(crate::lyrics::LyricsDocument::from_plain(
            &"x".repeat(100),
        )));
    app.state.lyrics.viewport_height = 6;
    app.state.active_panel = Panel::Lyrics;
    app.state
        .tick_frame(FrameMetrics::new(1, 1, 6, 4), Duration::ZERO);

    app.handle_command(Command::CursorBottom);

    assert_eq!(app.state().lyrics.scroll, 19);
    let logical_lines = app
        .state()
        .lyrics
        .document
        .as_ref()
        .expect("lyrics document")
        .lines
        .len();
    assert!(app.state().lyrics.scroll > logical_lines);
}

#[test]
fn artwork_resize_is_submitted_by_tick_not_size_queries() {
    let mut app = App::new();
    app.state_mut().artwork.set_enabled(true);
    app.state_mut().artwork.set_artwork(
        0,
        Some(crate::artwork::ArtworkProtocol::new(
            crate::artwork::testing::test_protocol(),
        )),
    );
    assert!(matches!(
        app.state().artwork.resize_state_for_test(),
        crate::artwork::ResizeLifecycle::Ready
    ));

    let _ = app
        .state()
        .artwork
        .image_size(ratatui::layout::Size::new(20, 10));
    assert!(matches!(
        app.state().artwork.resize_state_for_test(),
        crate::artwork::ResizeLifecycle::Ready
    ));

    let mut metrics = FrameMetrics::new(1, 1, 1, 1);
    metrics.artwork_target = Some(ratatui::layout::Size::new(20, 10));
    app.tick_frame(metrics, std::time::Instant::now());
    assert!(matches!(
        app.state().artwork.resize_state_for_test(),
        crate::artwork::ResizeLifecycle::Submitted { .. }
    ));
}

#[test]
fn focus_ring_with_lyrics_visible_walks_three_panels() {
    let mut app = playing_fixture(0);
    app.handle_command(Command::ToggleLyrics);
    assert_eq!(app.active_panel(), Panel::Lyrics);

    app.handle_command(Command::FocusNextPanel);
    assert_eq!(app.active_panel(), Panel::Playlist);
    app.handle_command(Command::FocusNextPanel);
    assert_eq!(app.active_panel(), Panel::Lyrics);
    app.handle_command(Command::FocusPreviousPanel);
    assert_eq!(app.active_panel(), Panel::Playlist);
    app.handle_command(Command::FocusPreviousPanel);
    assert_eq!(app.active_panel(), Panel::Browser);
    app.handle_command(Command::FocusPreviousPanel);
    assert_eq!(app.active_panel(), Panel::Lyrics);
}

#[test]
fn load_lyrics_effect_round_trips_local_results_through_the_bus() {
    let root = unique_temp_dir("lyrics-roundtrip");
    fs::write(root.join("song.mp3"), "audio").expect("audio fixture");
    fs::write(root.join("song.lrc"), "[00:01.00]cached line").expect("lrc fixture");

    let services = AppServices::new().expect("services construction");

    execute_effects(
        vec![Effect::LoadLyrics {
            track_index: 3,
            request: crate::lyrics::LyricsRequest {
                audio_path: root.join("song.mp3"),
                title: Some("Song".to_string()),
                artist: None,
                album: None,
            },
        }],
        &services,
        &pending_counter(),
    );

    match services.events().recv_timeout(EVENT_WAIT) {
        Ok(AppEvent::LyricsLoaded {
            track_index,
            operation_id,
            outcome,
            ..
        }) => {
            assert_eq!(track_index, 3);
            assert!(operation_id.get() > 0);
            assert_eq!(outcome.origin, Some(crate::lyrics::LyricsOrigin::Local));
            assert_eq!(
                outcome.document.as_ref().map(|d| d.lines[0].text.as_str()),
                Some("cached line")
            );
        }
        Ok(event) => panic!("expected LyricsLoaded, got {event:?}"),
        Err(error) => panic!("bus receive failed: {error}"),
    }

    services.shutdown();
}

#[test]
fn lyrics_screen_displays_title_from_metadata_when_available() {
    let mut app = App::new();
    app.state.extend_playlist(["/song.mp3"].map(PathBuf::from));
    app.state.playlist.select(0);
    app.state.playback.track_index = Some(0);
    app.state.playback.status = PlayStatus::Playing;
    app.state.lyrics.visible = true;

    let effects = app.begin_current_track();
    assert!(
        effects
            .iter()
            .any(|e| matches!(e, Effect::LoadLyrics { track_index: 0, .. }))
    );
    assert_eq!(app.state().lyrics.display_title.as_deref(), Some("song"));
}

#[test]
fn lyrics_request_drops_unknown_metadata_placeholders() {
    let metadata = TrackMetadata {
        title: "Black".to_string(),
        title_tagged: true,
        artist: "Unknown Artist".to_string(),
        album: "Unknown Album".to_string(),
        track_number: None,
        duration: Duration::from_secs(330),
        bitrate: None,
        sample_rate: None,
        codec: "MP3".to_string(),
        format: "MP3".to_string(),
        album_artist: None,
        disc_number: None,
        genre: None,
        year: None,
        composer: None,
        comment: None,
    };

    let (request, display_title) =
        App::lyrics_request_for(Path::new("/m/Pearl Jam - Black.mp3"), Some(&metadata));

    assert_eq!(request.title.as_deref(), Some("Black"));
    assert_eq!(
        request.artist, None,
        "Unknown Artist must not reach the lookup"
    );
    assert_eq!(
        request.album, None,
        "Unknown Album must not reach the lookup"
    );
    assert_eq!(display_title, "Black");
}

#[test]
fn lyrics_request_forwards_real_metadata() {
    let metadata = TrackMetadata {
        title: "Black".to_string(),
        title_tagged: true,
        artist: "Pearl Jam".to_string(),
        album: "Ten".to_string(),
        track_number: Some(3),
        duration: Duration::from_secs(336),
        bitrate: Some(320),
        sample_rate: Some(44100),
        codec: "MP3".to_string(),
        format: "MP3".to_string(),
        album_artist: None,
        disc_number: None,
        genre: None,
        year: None,
        composer: None,
        comment: None,
    };

    let (request, _) = App::lyrics_request_for(Path::new("/m/file.mp3"), Some(&metadata));

    assert_eq!(request.artist.as_deref(), Some("Pearl Jam"));
    assert_eq!(request.album.as_deref(), Some("Ten"));
}

/// A successful `StreamResolved` event appends the new stream track
/// to the queue, jumps the cursor onto it, closes the popup, and
/// surfaces a notification with the resolved name.
#[test]
fn apply_stream_resolved_appends_track_and_closes_dialog() {
    let url = url::Url::parse("https://radio.example.com/live").expect("valid url");
    let track = crate::track::Track::from_stream(url.clone(), crate::stream::StreamKind::Http);
    let mut app = App::from_config_and_store(
        KeysConfig::default(),
        PlaylistStore::for_dir(PathBuf::new()),
    );
    app.state_mut().active_panel = Panel::Playlist;
    // Open the Add Stream popup so apply_stream_resolved has a dialog
    // to close on success.
    app.handle_command(Command::AddStream);
    assert!(app.state().popup_dialog.active_popup_ref().is_none());
    let (request_id, _) = app.state_mut().begin_stream_resolution();

    let effects = app.apply_stream_resolved(request_id, url.clone(), Some(Box::new(track)), None);
    // No active named playlist in this test, so `append_autosave`
    // produces an empty `Vec`. The point of the assertion is that we
    // do not leak a half-built effect.
    assert!(effects.is_empty(), "the stream track is queued directly");
    assert_eq!(app.state().playlist.len(), 1);
    assert!(app.state().playlist.tracks()[0].is_stream());
    // The cursor jumped onto the new entry.
    assert_eq!(app.state().playlist.cursor(), 0);
    // The popup is gone.
    assert!(app.state().popup_dialog.active_popup_ref().is_none());
    assert!(app.state().popup_dialog.dialog_mode_ref().is_none());
    // A notification mentions the resolved track.
    let notification = app
        .state()
        .notifications
        .last()
        .expect("a notification was pushed");
    assert!(notification.contains("Added stream"), "got {notification}");
}

/// A failed `StreamResolved` keeps the popup open with the error
/// surfaced inside the dialog so the user can fix the URL and retry.
#[test]
fn apply_stream_resolved_failure_keeps_popup_open_with_error() {
    let url = url::Url::parse("https://radio.example.com/live").expect("valid url");
    let mut app = App::from_config_and_store(
        KeysConfig::default(),
        PlaylistStore::for_dir(PathBuf::new()),
    );
    app.state_mut().active_panel = Panel::Playlist;
    app.handle_command(Command::AddStream);
    assert!(app.state().popup_dialog.active_popup_ref().is_none());
    let (request_id, _) = app.state_mut().begin_stream_resolution();

    let effects = app.apply_stream_resolved(
        request_id,
        url.clone(),
        None,
        Some(WorkerError::message(
            "stream-resolve",
            "DNS resolution failed",
        )),
    );
    assert!(effects.is_empty());
    assert!(app.state().popup_dialog.active_popup_ref().is_none());
    match app.state().popup_dialog.dialog_mode_ref() {
        Some(DialogMode::AddStream { error, .. }) => {
            assert_eq!(error.as_deref(), Some("DNS resolution failed"));
        }
        other => panic!("expected AddStream dialog, got {other:?}"),
    }
    // The queue is untouched.
    assert_eq!(app.state().playlist.len(), 0);
}

/// A successful stream add against a *named* active playlist must
/// dispatch an `Effect::SaveActivePlaylist` whose rendered M3U body
/// contains the stream URL. This guards against the regression
/// where adding a stream left the persisted `.m3u8` stale until the
/// user manually saved again.
#[test]
fn apply_stream_resolved_dispatches_autosave_for_named_playlist() {
    let url = url::Url::parse("https://radio.example.com/live").expect("valid url");
    let track = crate::track::Track::from_stream(url.clone(), crate::stream::StreamKind::Http);
    let mut app = App::from_config_and_store(
        KeysConfig::default(),
        PlaylistStore::for_dir(PathBuf::new()),
    );
    // Pretend the user is editing the "Rock" playlist so the
    // autosave helper produces an effect.
    app.state_mut().active_playlist_name = Some("Rock".to_string());
    app.state_mut().active_panel = Panel::Playlist;
    app.handle_command(Command::AddStream);
    assert!(app.state().popup_dialog.active_popup_ref().is_none());
    let (request_id, _) = app.state_mut().begin_stream_resolution();

    let effects = app.apply_stream_resolved(request_id, url.clone(), Some(Box::new(track)), None);

    let saved = effects
        .iter()
        .find_map(|effect| match effect {
            Effect::SaveActivePlaylist { name, contents } => Some((name, contents)),
            _ => None,
        })
        .expect("a named active playlist must produce a SaveActivePlaylist effect");
    assert_eq!(saved.0, "Rock");
    assert!(
        saved.1.contains(url.as_str()),
        "the rendered M3U must contain the stream URL, got:\n{}",
        saved.1
    );
    assert!(
        saved.1.contains("#EXTM3U"),
        "the rendered M3U must carry the EXTM3U header"
    );
}
