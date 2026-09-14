//! User-facing audio output abstraction, decoupled from any concrete backend.
//!
//! The UI and the settings layer only ever see [`AudioOutput`] values and an
//! [`OutputProvider`]; the underlying discovery (ALSA, cpal, PipeWire) stays
//! behind the provider so the renderer never depends on a terminal session.

use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::fmt;
use std::rc::Rc;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

/// A user-facing audio output (a PipeWire playback sink).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioOutput {
    /// Stable sink identifier. An empty string means "let the system pick the
    /// session default sink". Never store the ephemeral numeric ID shown by
    /// `wpctl`; prefer `node.name`/`device.name` when available.
    pub id: String,
    /// Short display name shown in the UI.
    pub name: String,
    /// Longer human readable description, when known.
    pub description: String,
    /// Whether this is the current PipeWire/WirePlumber default sink.
    pub is_default: bool,
    /// Whether the sink is currently available.
    pub available: bool,
    /// The PipeWire node (global) id, used to rebuild the output stream on a
    /// different sink when the user changes the active device.
    pub node_id: Option<u32>,
}

impl AudioOutput {
    /// The sentinel entry that delegates the choice to the session default.
    pub fn session_default() -> Self {
        Self {
            id: String::new(),
            name: "Default".to_string(),
            description: "System default output".to_string(),
            is_default: true,
            available: true,
            node_id: None,
        }
    }
}

/// The routing identity for one output selection.
///
/// `stable_id` is the PipeWire `node.name` persisted in the user config and
/// used for routing. `node_id` is only the current registry id from the latest
/// enumeration; it is diagnostic and must not be used as the connection target.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OutputTarget {
    pub stable_id: Option<String>,
    pub node_id: Option<u32>,
}

impl From<&AudioOutput> for OutputTarget {
    fn from(output: &AudioOutput) -> Self {
        Self {
            stable_id: (!output.id.is_empty()).then(|| output.id.clone()),
            node_id: output.node_id,
        }
    }
}

/// Supplies the list of outputs and validates the user's selection.
pub trait OutputProvider: fmt::Debug + Send + Sync {
    /// Return the most recent enumeration without starting discovery.
    ///
    /// `None` means the caller should display a placeholder until a fresh
    /// enumeration completes asynchronously.
    fn cached_outputs(&self) -> Option<Vec<AudioOutput>> {
        None
    }

    /// Enumerate the currently available outputs. The list must always include
    /// the session default entry so the user can revert the choice.
    fn list_outputs(&self) -> Vec<AudioOutput>;
    /// The stable id of the current session default sink, if it can be known.
    fn default_output_id(&self) -> Option<String>;
    /// Accept a stable output id for settings and startup selection.
    ///
    /// This validates the most recent valid provider view without starting a
    /// discovery operation. Live routing belongs to
    /// [`crate::audio::AudioCommand::SetOutput`] and the audio worker.
    fn accept_selection(&self, id: &str) -> anyhow::Result<()>;
}

/// Output selected while replaying the persisted preference at startup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartupOutputSelection {
    /// The output used for the worker command.
    pub output: AudioOutput,
    /// Whether the persisted id was unavailable and the session default won.
    pub used_session_default: bool,
}

/// Resolve a persisted stable output id without touching a live audio stream.
///
/// An empty persisted id already means session default, so discovery is
/// skipped. A non-empty id is matched only against currently available sinks;
/// missing sinks explicitly fall back to the default while the caller keeps
/// the original persisted preference unchanged.
pub fn select_startup_output(
    provider: &dyn OutputProvider,
    persisted_id: &str,
) -> StartupOutputSelection {
    if persisted_id.is_empty() {
        return StartupOutputSelection {
            output: AudioOutput::session_default(),
            used_session_default: false,
        };
    }

    let output = provider
        .list_outputs()
        .into_iter()
        .find(|output| output.id == persisted_id && output.available);

    match output {
        Some(output) => StartupOutputSelection {
            output,
            used_session_default: false,
        },
        None => StartupOutputSelection {
            output: AudioOutput::session_default(),
            used_session_default: true,
        },
    }
}

/// Trivial provider that only exposes the session default. Used as a safe
/// fallback until a real backend (e.g. native PipeWire) is wired in.
#[derive(Debug, Default)]
pub struct NullOutputProvider;

impl OutputProvider for NullOutputProvider {
    fn cached_outputs(&self) -> Option<Vec<AudioOutput>> {
        Some(self.list_outputs())
    }

    fn list_outputs(&self) -> Vec<AudioOutput> {
        vec![AudioOutput::session_default()]
    }

    fn default_output_id(&self) -> Option<String> {
        None
    }

    fn accept_selection(&self, id: &str) -> anyhow::Result<()> {
        if id.is_empty() {
            Ok(())
        } else {
            Err(anyhow::anyhow!("output is not available: {id}"))
        }
    }
}

/// A fixed list provider used by tests and simple setups.
#[derive(Debug, Clone)]
pub struct StaticOutputProvider {
    outputs: Vec<AudioOutput>,
    default_id: Option<String>,
}

impl StaticOutputProvider {
    pub fn new(outputs: Vec<AudioOutput>, default_id: Option<String>) -> Self {
        Self {
            outputs,
            default_id,
        }
    }
}

impl OutputProvider for StaticOutputProvider {
    fn cached_outputs(&self) -> Option<Vec<AudioOutput>> {
        Some(self.outputs.clone())
    }

    fn list_outputs(&self) -> Vec<AudioOutput> {
        self.outputs.clone()
    }

    fn default_output_id(&self) -> Option<String> {
        self.default_id.clone()
    }

    fn accept_selection(&self, id: &str) -> anyhow::Result<()> {
        accept_selection_from_outputs(&self.outputs, id)
    }
}

/// Shared handle to the provider passed between the app layers.
pub type ArcOutputProvider = Arc<dyn OutputProvider>;

/// Provider that discovers playback sinks through the native PipeWire API.
///
/// The connection is created and torn down inside each call (the crate's high
/// level handles are `Rc`-based and cannot live across threads), so this
/// provider stays `Send + Sync` and the discovery cost is paid only when the
/// settings or startup selection is requested.
#[derive(Debug, Default)]
pub struct PipeWireOutputProvider;

/// How long a discovered sink list stays valid before re-enumerating.
const OUTPUTS_CACHE_TTL: Duration = Duration::from_secs(5);

/// Process-wide cache of the last enumerated sink list, shared by every
/// provider call so a re-opened Settings tab does not re-discover sinks.
static OUTPUT_CACHE: LazyLock<Mutex<OutputCache>> =
    LazyLock::new(|| Mutex::new(OutputCache::default()));

/// Serializes cache misses so concurrent settings visits do not start duplicate
/// PipeWire registry traversals. Cache readers never wait for discovery to
/// finish.
static OUTPUT_ENUMERATION_LOCK: Mutex<()> = Mutex::new(());

#[derive(Debug, Default)]
struct OutputCache {
    entry: Option<(Instant, Vec<AudioOutput>)>,
}

impl OutputCache {
    fn fresh(&self, now: Instant) -> Option<Vec<AudioOutput>> {
        self.entry.as_ref().and_then(|(at, outputs)| {
            (now.duration_since(*at) < OUTPUTS_CACHE_TTL).then(|| outputs.clone())
        })
    }

    fn store(&mut self, at: Instant, outputs: Vec<AudioOutput>) {
        self.entry = Some((at, outputs));
    }
}

impl OutputProvider for PipeWireOutputProvider {
    fn cached_outputs(&self) -> Option<Vec<AudioOutput>> {
        OUTPUT_CACHE
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .fresh(Instant::now())
    }

    fn list_outputs(&self) -> Vec<AudioOutput> {
        list_outputs_with_cache(
            &OUTPUT_CACHE,
            &OUTPUT_ENUMERATION_LOCK,
            enumerate_output_list,
            Instant::now,
        )
    }

    fn default_output_id(&self) -> Option<String> {
        // The session default is delegated to the "Default" entry; the actual
        // default sink can be resolved from PipeWire metadata in a later slice.
        None
    }

    fn accept_selection(&self, id: &str) -> anyhow::Result<()> {
        accept_selection_from_cache(self.cached_outputs(), id)
    }
}

fn list_outputs_with_cache<F, N>(
    cache: &Mutex<OutputCache>,
    enumeration_lock: &Mutex<()>,
    enumerate: F,
    now: N,
) -> Vec<AudioOutput>
where
    F: FnOnce() -> Vec<AudioOutput>,
    N: Fn() -> Instant,
{
    let _enumeration_guard = enumeration_lock
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let lookup_at = now();
    if let Some(outputs) = cache
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .fresh(lookup_at)
    {
        return outputs;
    }

    let outputs = enumerate();
    cache
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .store(now(), outputs.clone());
    outputs
}

fn accept_selection_from_cache(
    cached_outputs: Option<Vec<AudioOutput>>,
    id: &str,
) -> anyhow::Result<()> {
    if id.is_empty() {
        return Ok(());
    }
    let Some(outputs) = cached_outputs else {
        return Err(anyhow::anyhow!("output is not available: {id}"));
    };
    accept_selection_from_outputs(&outputs, id)
}

fn accept_selection_from_outputs(outputs: &[AudioOutput], id: &str) -> anyhow::Result<()> {
    if outputs
        .iter()
        .any(|output| output.id == id && output.available)
    {
        Ok(())
    } else {
        Err(anyhow::anyhow!("output is not available: {id}"))
    }
}

/// A stable, user-facing output built from PipeWire node properties.
///
/// Kept as a pure mapping so it can be unit tested without a PipeWire session.
pub(crate) fn audio_output_from_props(
    node_name: Option<&str>,
    node_description: Option<&str>,
    device_name: Option<&str>,
    node_id: Option<u32>,
) -> Option<AudioOutput> {
    let id = node_name.unwrap_or("");
    if id.is_empty() {
        return None;
    }
    let label = node_description
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .or_else(|| device_name.map(str::trim).filter(|s| !s.is_empty()))
        .unwrap_or(id)
        .to_string();
    Some(AudioOutput {
        id: id.to_string(),
        name: label.clone(),
        description: label,
        is_default: false,
        available: true,
        node_id,
    })
}

/// Connect to PipeWire and collect every `Audio/Sink` node as an output.
fn enumerate_output_list() -> Vec<AudioOutput> {
    let mut outputs = vec![AudioOutput::session_default()];
    outputs.extend(enumerate_audio_sinks());
    // De-duplicate by stable id, now that the full list is available.
    let mut seen = HashSet::new();
    outputs.retain(|out| seen.insert(out.id.clone()));
    outputs
}

const OUTPUT_ENUMERATION_ITERATION: Duration = Duration::from_millis(5);
const OUTPUT_ENUMERATION_MAX_DURATION: Duration = Duration::from_millis(250);
const OUTPUT_ENUMERATION_NO_PROGRESS_LIMIT: usize = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RegistryIteration {
    Progress,
    NoProgress,
    Complete,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RegistryWaitResult {
    Complete,
    NoProgress,
    TimedOut,
}

trait RegistryDriver {
    fn iterate(&mut self, timeout: Duration) -> RegistryIteration;
}

struct PipeWireRegistryDriver<'a> {
    main_loop: &'a pipewire::main_loop::MainLoopRc,
    completed: Rc<Cell<bool>>,
    observed_events: Rc<Cell<u64>>,
}

impl RegistryDriver for PipeWireRegistryDriver<'_> {
    fn iterate(&mut self, timeout: Duration) -> RegistryIteration {
        let before = self.observed_events.get();
        self.main_loop
            .loop_()
            .iterate(pipewire::loop_::Timeout::Finite(timeout));
        if self.completed.get() {
            RegistryIteration::Complete
        } else if self.observed_events.get() != before {
            RegistryIteration::Progress
        } else {
            RegistryIteration::NoProgress
        }
    }
}

fn wait_for_registry_completion<D, N>(driver: &mut D, mut now: N) -> RegistryWaitResult
where
    D: RegistryDriver,
    N: FnMut() -> Instant,
{
    let started = now();
    let mut no_progress = 0;
    loop {
        let elapsed = now().saturating_duration_since(started);
        if elapsed >= OUTPUT_ENUMERATION_MAX_DURATION {
            return RegistryWaitResult::TimedOut;
        }
        let timeout = OUTPUT_ENUMERATION_ITERATION.min(OUTPUT_ENUMERATION_MAX_DURATION - elapsed);
        match driver.iterate(timeout) {
            RegistryIteration::Complete => return RegistryWaitResult::Complete,
            RegistryIteration::Progress => no_progress = 0,
            RegistryIteration::NoProgress => {
                no_progress += 1;
                if no_progress >= OUTPUT_ENUMERATION_NO_PROGRESS_LIMIT {
                    return RegistryWaitResult::NoProgress;
                }
            }
        }
    }
}

fn enumerate_audio_sinks() -> Vec<AudioOutput> {
    // pipewire::init must run once per process; guard it so a reconnect cannot
    // be called while the crate is already initialised.
    static INIT: std::sync::Once = std::sync::Once::new();
    INIT.call_once(pipewire::init);

    let shared = Rc::new(RefCell::new(Vec::<AudioOutput>::new()));
    let seen = Rc::new(RefCell::new(HashSet::<String>::new()));
    let observed_events = Rc::new(Cell::new(0u64));

    let Ok(main_loop) = pipewire::main_loop::MainLoopRc::new(None) else {
        return Vec::new();
    };
    let context = match pipewire::context::ContextRc::new(&main_loop, None) {
        Ok(context) => context,
        Err(_) => return Vec::new(),
    };
    let core = match context.connect_rc(None) {
        Ok(core) => core,
        Err(_) => return Vec::new(),
    };
    let registry = match core.get_registry_rc() {
        Ok(registry) => registry,
        Err(_) => return Vec::new(),
    };

    let shared_global = shared.clone();
    let seen_global = seen.clone();
    let observed_global = observed_events.clone();
    let _listener = registry
        .add_listener_local()
        .global(move |obj| {
            observed_global.set(observed_global.get().saturating_add(1));
            if obj.type_ != pipewire::types::ObjectType::Node {
                return;
            }
            let Some(props) = obj.props else {
                return;
            };
            if props.get(*pipewire::keys::MEDIA_CLASS) != Some("Audio/Sink") {
                return;
            }
            let node_name = props.get(*pipewire::keys::NODE_NAME);
            let node_description = props.get(*pipewire::keys::NODE_DESCRIPTION);
            let device_name = props.get(*pipewire::keys::DEVICE_NAME);
            let node_id = obj.id;
            let Some(output) =
                audio_output_from_props(node_name, node_description, device_name, Some(node_id))
            else {
                return;
            };
            if seen_global.borrow_mut().insert(output.id.clone()) {
                shared_global.borrow_mut().push(output);
            }
        })
        .register();

    let completed = Rc::new(Cell::new(false));
    let expected_sync_seq = Rc::new(Cell::new(None));
    let completed_for_listener = completed.clone();
    let expected_for_listener = expected_sync_seq.clone();
    let _core_listener = core
        .add_listener_local()
        .done(move |_id, seq| {
            if expected_for_listener.get() == Some(seq.seq()) {
                completed_for_listener.set(true);
            }
        })
        .register();
    let Ok(sync_seq) = core.sync(0) else {
        return Vec::new();
    };
    expected_sync_seq.set(Some(sync_seq.seq()));

    let mut driver = PipeWireRegistryDriver {
        main_loop: &main_loop,
        completed,
        observed_events,
    };
    let _ = wait_for_registry_completion(&mut driver, Instant::now);

    let outputs = shared.borrow().clone();
    drop(core);
    drop(context);
    outputs
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug)]
    struct FakeRegistry {
        steps: Vec<RegistryIteration>,
        next: usize,
        elapsed: Rc<Cell<Duration>>,
    }

    impl RegistryDriver for FakeRegistry {
        fn iterate(&mut self, timeout: Duration) -> RegistryIteration {
            self.elapsed.set(self.elapsed.get() + timeout);
            let step = self
                .steps
                .get(self.next)
                .copied()
                .unwrap_or(RegistryIteration::NoProgress);
            self.next += 1;
            step
        }
    }

    fn fake_output(id: &str, available: bool) -> AudioOutput {
        AudioOutput {
            id: id.to_string(),
            name: id.to_string(),
            description: String::new(),
            is_default: false,
            available,
            node_id: None,
        }
    }

    #[test]
    fn null_provider_only_offers_session_default() {
        let provider = NullOutputProvider;
        let outputs = provider.list_outputs();
        assert_eq!(outputs.len(), 1);
        assert_eq!(outputs[0].id, "");
        assert_eq!(outputs[0].name, "Default");
        assert!(provider.accept_selection("").is_ok());
        assert!(provider.accept_selection("missing").is_err());
    }

    #[test]
    fn static_provider_returns_models_and_marks_default() {
        let outputs = vec![
            AudioOutput::session_default(),
            AudioOutput {
                id: "alsa_output.card.sink".to_string(),
                name: "Ryzen Analog".to_string(),
                description: "Ryzen HD Audio Analog Stereo".to_string(),
                is_default: false,
                available: true,
                node_id: Some(33),
            },
        ];
        let provider =
            StaticOutputProvider::new(outputs.clone(), Some("alsa_output.card.sink".to_string()));
        assert_eq!(provider.list_outputs(), outputs);
        assert_eq!(
            provider.default_output_id().as_deref(),
            Some("alsa_output.card.sink")
        );
        assert!(provider.accept_selection("alsa_output.card.sink").is_ok());
        assert!(provider.accept_selection("missing").is_err());
    }

    #[test]
    fn session_default_is_the_sentinel_for_system_choice() {
        let d = AudioOutput::session_default();
        assert_eq!(d.id, "");
        assert!(d.is_default);
    }

    #[test]
    fn startup_replay_selects_an_available_persisted_output() {
        let provider = StaticOutputProvider::new(
            vec![
                AudioOutput::session_default(),
                AudioOutput {
                    id: "sink-a".to_string(),
                    name: "Sink A".to_string(),
                    description: String::new(),
                    is_default: false,
                    available: true,
                    node_id: Some(57),
                },
            ],
            None,
        );

        let selection = select_startup_output(&provider, "sink-a");

        assert_eq!(selection.output.id, "sink-a");
        assert_eq!(selection.output.node_id, Some(57));
        assert!(!selection.used_session_default);
    }

    #[test]
    fn startup_replay_falls_back_without_discarding_unavailable_preference() {
        let provider = StaticOutputProvider::new(vec![AudioOutput::session_default()], None);

        let selection = select_startup_output(&provider, "disconnected-sink");

        assert_eq!(selection.output, AudioOutput::session_default());
        assert!(selection.used_session_default);
    }

    #[test]
    fn startup_replay_uses_session_default_without_discovery_for_empty_preference() {
        let provider = NullOutputProvider;

        let selection = select_startup_output(&provider, "");

        assert_eq!(selection.output, AudioOutput::session_default());
        assert!(!selection.used_session_default);
    }

    #[test]
    fn maps_pipewire_node_props_to_output() {
        let out = audio_output_from_props(
            Some("alsa_output.pci.sink"),
            Some("Ryzen HD Audio Controller Analog Stereo"),
            Some("device"),
            Some(33),
        )
        .expect("sink maps");
        assert_eq!(out.id, "alsa_output.pci.sink");
        assert_eq!(out.name, "Ryzen HD Audio Controller Analog Stereo");
        assert_eq!(out.node_id, Some(33));
        assert!(out.available);
        assert!(!out.is_default);
    }

    #[test]
    fn prefers_description_then_device_then_node_name() {
        let with_desc =
            audio_output_from_props(Some("n"), Some("desc"), Some("dev"), None).unwrap();
        assert_eq!(with_desc.name, "desc");
        let with_dev = audio_output_from_props(Some("n"), None, Some("dev"), None).unwrap();
        assert_eq!(with_dev.name, "dev");
        let only_name = audio_output_from_props(Some("n"), None, None, None).unwrap();
        assert_eq!(only_name.name, "n");
    }

    #[test]
    fn rejects_node_without_a_name() {
        assert!(audio_output_from_props(None, Some("desc"), None, None).is_none());
    }

    #[test]
    fn registry_completion_stops_on_done_without_consuming_the_timeout_budget() {
        let elapsed = Rc::new(Cell::new(Duration::ZERO));
        let mut registry = FakeRegistry {
            steps: vec![RegistryIteration::Progress, RegistryIteration::Complete],
            next: 0,
            elapsed: elapsed.clone(),
        };
        let started = Instant::now();

        let result = wait_for_registry_completion(&mut registry, || started + elapsed.get());

        assert_eq!(result, RegistryWaitResult::Complete);
        assert_eq!(registry.next, 2);
        assert!(elapsed.get() < OUTPUT_ENUMERATION_MAX_DURATION);
    }

    #[test]
    fn registry_completion_stops_after_consecutive_no_progress() {
        let elapsed = Rc::new(Cell::new(Duration::ZERO));
        let mut registry = FakeRegistry {
            steps: vec![RegistryIteration::NoProgress; OUTPUT_ENUMERATION_NO_PROGRESS_LIMIT],
            next: 0,
            elapsed: elapsed.clone(),
        };
        let started = Instant::now();

        let result = wait_for_registry_completion(&mut registry, || started + elapsed.get());

        assert_eq!(result, RegistryWaitResult::NoProgress);
        assert_eq!(registry.next, OUTPUT_ENUMERATION_NO_PROGRESS_LIMIT);
        assert_eq!(
            elapsed.get(),
            OUTPUT_ENUMERATION_ITERATION * OUTPUT_ENUMERATION_NO_PROGRESS_LIMIT as u32
        );
        assert!(elapsed.get() < OUTPUT_ENUMERATION_MAX_DURATION);
    }

    #[test]
    fn registry_completion_stops_at_the_maximum_duration_even_with_progress() {
        let elapsed = Rc::new(Cell::new(Duration::ZERO));
        let mut registry = FakeRegistry {
            steps: vec![RegistryIteration::Progress; 100],
            next: 0,
            elapsed: elapsed.clone(),
        };
        let started = Instant::now();

        let result = wait_for_registry_completion(&mut registry, || started + elapsed.get());

        assert_eq!(result, RegistryWaitResult::TimedOut);
        assert_eq!(elapsed.get(), OUTPUT_ENUMERATION_MAX_DURATION);
        assert_eq!(
            registry.next,
            (OUTPUT_ENUMERATION_MAX_DURATION.as_millis() / OUTPUT_ENUMERATION_ITERATION.as_millis())
                as usize
        );
    }

    #[test]
    fn cache_miss_enumerates_hit_reuses_and_expiry_refreshes() {
        let cache = Mutex::new(OutputCache::default());
        let enumeration_lock = Mutex::new(());
        let clock = Cell::new(Instant::now());
        let calls = Cell::new(0);
        let output = fake_output("sink-a", true);

        let first = list_outputs_with_cache(
            &cache,
            &enumeration_lock,
            || {
                calls.set(calls.get() + 1);
                vec![output.clone()]
            },
            || clock.get(),
        );
        assert_eq!(first, vec![output.clone()]);
        assert_eq!(calls.get(), 1, "a cache miss must enumerate once");

        let hit = list_outputs_with_cache(
            &cache,
            &enumeration_lock,
            || {
                calls.set(calls.get() + 1);
                vec![fake_output("unexpected", true)]
            },
            || clock.get(),
        );
        assert_eq!(hit, vec![output.clone()]);
        assert_eq!(calls.get(), 1, "a fresh cache hit must not enumerate");

        clock.set(clock.get() + OUTPUTS_CACHE_TTL);
        let refreshed = list_outputs_with_cache(
            &cache,
            &enumeration_lock,
            || {
                calls.set(calls.get() + 1);
                vec![fake_output("sink-b", true)]
            },
            || clock.get(),
        );
        assert_eq!(refreshed[0].id, "sink-b");
        assert_eq!(calls.get(), 2, "an expired cache must enumerate again");
    }

    #[test]
    fn expired_cache_is_not_returned_as_a_valid_cached_view() {
        let now = Instant::now();
        let mut cache = OutputCache::default();
        cache.store(now, vec![fake_output("sink-a", true)]);

        assert!(cache.fresh(now).is_some());
        assert!(cache.fresh(now + OUTPUTS_CACHE_TTL).is_none());
    }

    #[test]
    fn selection_uses_the_cached_view_and_preserves_available_failure_behavior() {
        let outputs = vec![
            AudioOutput::session_default(),
            fake_output("sink-a", true),
            fake_output("sink-b", false),
        ];

        assert!(accept_selection_from_cache(Some(outputs.clone()), "sink-a").is_ok());
        assert!(accept_selection_from_cache(Some(outputs), "sink-b").is_err());
        assert!(accept_selection_from_cache(None, "sink-a").is_err());
        assert!(accept_selection_from_cache(None, "").is_ok());
    }
}
