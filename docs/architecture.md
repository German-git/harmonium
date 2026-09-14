# Architecture

System design, module structure, data flow, and design patterns for Harmonium.

## Module map

```
src/
├── main.rs              Entry point, terminal init, splash run, event loop, picker construction
├── lib.rs               Public module re-exports, APP_VERSION constant
├── app/
│   ├── mod.rs           App state machine, command dispatch, effect emission
│   ├── settings.rs      Settings draft helpers (value translation, binding validation)
│   └── dialogs.rs       Playlist naming dialog resolution against the store
├── state.rs             AppState, Panel, Popup, SettingsTab, DialogMode, notification buffer
├── command.rs           Command enum (52 variants) — the application vocabulary
├── input.rs             InputMapper, key routing (dialog → global → panel), keymap summary
├── event.rs             Bounded EventBus, AppEvent variants, EffectErrorKind
├── search.rs            Contextual browser/playlist matching and stable SearchResult values
├── runtime.rs           AppServices (Tokio runtime, audio handle, artwork loader)
├── config.rs            Paths (XDG), AppConfig (TOML), PersistedState (state.toml)
├── error.rs             Domain errors (thiserror)
├── playback_mode.rs     PlaybackMode (RepeatMode × shuffle bool)
├── browser_state.rs     BrowserState (entries, cursor, scroll)
├── track.rs             Track metadata and path
├── test_support.rs      Temp dir helpers for tests
│
├── ui/
│   ├── mod.rs           Frame metrics plus draw-only orchestration (compute_layout → panels → popups)
│   ├── layout.rs        compute_layout: fixed vertical bands, compact threshold
│   ├── panels/
│   │   ├── mod.rs       draw_browser, draw_playlist, draw_now_playing, draw_lyrics_panel,
│   │   │                draw_footer, draw_artwork_over_playlist/browser
│   │   ├── popups.rs    draw_confirm_quit_popup, draw_help_popup,
│   │   │                draw_playlist_manager_popup, draw_confirm_overwrite_popup,
│   │   │                draw_naming_dialog
│   │   └── settings.rs  draw_settings (full-window five-tab editor)
│   └── theme.rs         Theme struct (24 color fields), TOML theme loading
│
├── audio/
│   ├── mod.rs           Re-exports, format_clock, progress_ratio
│   ├── engine.rs        Audio worker thread (rodio decode, SoundTouch tempo, sink mgmt)
│   ├── pipewire_sink.rs Native PipeWire output stream (dedicated real-time thread)
│   ├── output.rs        AudioOutput model + OutputProvider (sink enumeration)
│   ├── resample.rs      Linear-interpolation adapter for crossfade preloading
│   ├── error.rs         AudioError (thiserror)
│   └── playback.rs      PlaybackState, PlayStatus, PlaybackSnapshot

├── stream/
│   ├── mod.rs             StreamResolver and provider exports
│   ├── resolver.rs        Ordered provider selection and metadata fallbacks
│   ├── provider.rs        StreamProvider, ResolvedStream, StreamReader, errors
│   ├── source.rs          TrackSource and StreamKind
│   ├── http.rs            Generic HTTP stream provider
│   ├── hls.rs             Buffered HLS provider
│   ├── radio_browser.rs   Radio Browser station provider
│   └── youtube.rs         yt-dlp-backed YouTube provider
│
├── lyrics/
│   ├── mod.rs           LyricsService: source chain Local → Metadata → Remote, memo
│   ├── locator.rs       Recursive local `.lrc` search
│   ├── metadata.rs      Embedded synced-lyrics tag reading
│   ├── remote.rs        LRCLIB provider (off unless enabled)
│   ├── cache.rs         Saves remote lyrics beside the audio file (`lyrics/` folder)
│   ├── lrc.rs           LRC parse/format (timed and enhanced)
│   └── karaoke.rs       Active-line timing, follow-scroll, document layout
│
├── artwork/
│   ├── mod.rs           ArtworkLoader (Arc<Picker>), ArtworkState (RefCell protocol),
│   │                    ArtworkProtocol bus newtype
│   ├── backend.rs       Backend selection, external lifecycle, and fallback transitions
│   ├── ueberzugpp.rs    ueberzugpp external image layer
│   ├── ueberzug.rs      Legacy ueberzug external image layer
│   ├── loader.rs        Source resolution: embedded → cover files → remote → None
│   └── remote.rs        MusicBrainz / Cover Art Archive remote artwork fetch (reqwest + sha2)
│
├── filesystem/
│   ├── mod.rs           Re-exports
│   ├── entry.rs         FileEntry, EntryKind, audio extension detection
│   ├── browser.rs       BrowserState helpers, start-directory resolution
│   └── scanner.rs       Directory scanning (async via spawn_background)
│
├── metadata/
│   ├── mod.rs           Re-exports
│   └── reader.rs        TrackMetadata extraction via lofty
│
├── playlist/
│   ├── mod.rs           Re-exports
│   ├── playlist.rs      Playlist (Vec<Track>, cursor)
│   ├── manager.rs       PlaylistStore (.m3u8 persistence with #EXTINF)
│   ├── navigation.rs    NavigationState (shuffle pools, history, selection engine)
│   └── sorter.rs        Queue reordering by file name or metadata fields
│
└── splash/
    ├── mod.rs           run(&Theme, &EventSender) → Outcome, timeline phases, ENABLED switch
    ├── animation.rs     Pure scanline/reveal model (testable without a TTY)
    └── logo.rs          Static ASCII "harmonium" logo constant
```

`draw_status_line` still exists in `ui/panels/mod.rs` but is no longer
called: the status is rendered inside the Now Playing band.

`App::tick_frame` is the state-update boundary for frame-derived values. It
advances the shared spinner, updates viewport geometry, prepares lyrics layout,
and applies directional lyrics follow before drawing. `ui::render` then consumes
the immutable `App` view and only draws the frame; repeated draws cannot advance
or re-anchor application state.

## Layering rules

These are intended boundaries enforced by code review, not by the compiler.
The current implementation has the following deliberate or transitional
exceptions:

- **UI** (`ui/`) must never see `rodio`, `symphonia`, `tokio`, or `ratatui-image` types directly.
  The only exceptions are `src/artwork/` (owns the `ratatui-image` boundary) and
  `src/state.rs` (holds `ArtworkState`); panel code calls the `ArtworkState::render`
  facade without importing the image widget type.
- **Domain** modules (`audio`, `artwork`, `filesystem`, `metadata`, `playlist`, `lyrics`)
  must not depend on each other's internal types — they communicate through
  `AppState` and `Effect`/`AppEvent`.
- **AppState filesystem access** is an intended boundary, not a fully enforced
  one: `AppState::change_browser_dir` applies a directory and preloaded entries,
  while `Effect::ScanDirectory` performs the current directory listing in a
  background operation. Tests and setup helpers may still construct state with
  direct filesystem fixtures.
- **Audio backend** types are intended to stay in `src/audio/`: rodio decoding
  and the SoundTouch tempo processor live in `engine.rs`, and the PipeWire FFI
  lives in `pipewire_sink.rs`. `AudioEngineHandle` is the facade visible to
  `app/`, but the current engine also consumes the stream layer's
  `TrackSource`, `StreamResolver`, and `StreamReader` directly for playback.

## Data flow

```
Keyboard
   ↓
crossterm Event
   ↓
InputMapper::map_key(context) → Command
   ↓
App::handle_command(command) → Vec<Effect>
    ↓
App::execute_effects(services)
    ├── ScanDirectory → spawn_background → AppEvent::ScanCompleted
    ├── LoadMetadata  → spawn_background → AppEvent::MetadataCompleted
    ├── Search        → spawn_background → AppEvent::SearchCompleted
    ├── ResolveStream → cancellable spawn_blocking → AppEvent::StreamResolved (critical)
    ├── LoadArtwork   → spawn_background → AppEvent::ArtworkLoaded
    ├── LoadLyrics    → spawn_background → AppEvent::LyricsLoaded
    └── Audio(cmd)    → AudioCommand channel → audio worker → PipeWire sink
                              │
                               └── stream TrackSource → detached acquisition thread
                                   → StreamResolver/providers → StreamReader
                                   → decoder → SourceReady/SourceFailed
                              ↓
                      AppEvent::PlaybackProgress
                      AppEvent::TrackEnded
                      AppEvent::CrossfadeCompleted
    ↓
TerminalReader + EventBus → dirty event loop (main.rs)
    ↓
apply_* methods → update AppState
    ↓
App::tick_frame(metrics, elapsed)
    ↓
ui::render(frame, &app, theme) → draw_* functions
```

The event loop is event-driven with dirty-frame rendering:

1. `TerminalReader` owns blocking crossterm input and publishes key/resize events
   to the bounded `EventBus`.
2. When the frame is clean, the main thread waits with `EventBus::recv_timeout`.
   Worker publications or terminal input wake it; the timeout is retained for
   spinner and playback-clock ticks.
3. A dirty iteration drains the pending event burst, applies operation and
   identity gates, runs artwork resize completion, calls `App::tick_frame`, and
   draws once. No event is processed from inside `ui::render`.

## Concurrency model

```
Harmonium
    │
    ├── TUI (main thread, synchronous)
    │     └── Ratatui draw cycle, crossterm event polling
    │
    ├── Audio worker (dedicated thread: rodio/Symphonia decode,
    │   │   SoundTouch tempo, sink management)
    │   │   └── AudioCommand channel, sends PlaybackProgress/TrackEnded/
    │   │       CrossfadeCompleted
    │   └── PipeWire sink (own real-time thread feeding the output stream)
    │
    └── Tokio runtime (multi-thread, in AppServices)
          ├── Directory scanning (spawn_background)
          ├── Metadata extraction (spawn_blocking)
          ├── Artwork decode (spawn_blocking)
          └── Lyrics resolution (spawn_blocking)
                 └── Results → bounded EventBus → main thread
```

### Audio playback flow

```text
UI command/effect
  → AudioEngineHandle
  → AudioCommand channel
  → dedicated audio Worker thread
      → local decoder or detached stream acquisition
      → Decoder / source samples
      → SoundTouch when non-default tempo is active
      → PipeWireSink sample pool
  → PipeWire stream thread
      → RT process callback
      → negotiated device sink
```

The worker owns decoding, source-frame accounting, tempo processing, sink
creation, and command ordering. SoundTouch is a worker-owned time-stretch
pipeline; its input and output accounting remain separate so its buffered tail
can drain before a track ends. `PipeWireSink` owns a bounded preallocated sample
pool and a dedicated PipeWire thread. Its real-time process callback drains
control commands, acknowledges flushes only after queued samples are discarded,
fills negotiated F32LE buffers without allocation, applies the atomic gain, and
publishes played-frame progress. Output-device changes rebuild the stream on the
worker thread rather than reconnecting from the callback, because PipeWire
reconnection from the process callback can corrupt internal state and crash.

**Bridge pattern**: `AppServices::spawn_background` converts Tokio task
results and panics into `AppEvent::Notification` through the existing
the bounded `EventBus`, tagging each failure with an `EffectErrorKind`
(`Audio`, `Task`, `Playlist`). Critical completion, notification, and
state-transition events use the bus's blocking critical path; only tick,
resize, and playback progress are coalesced. This bridges the async Tokio
world into the synchronous TUI without requiring the TUI to understand async.

**Single runtime**: one `tokio::runtime::Runtime` is created in `AppServices::new()`
before the terminal enters raw mode. All background work shares this runtime.

## Design patterns

| Pattern | Where | Why |
|---------|-------|-----|
| **Command** | `command.rs`, `input.rs`, `app/` | Key → `Command` enum (52 variants) → execution. Input decoupled from logic |
| **Event Bus (Observer)** | `event.rs` | Workers publish `AppEvent` over a bounded queue; critical events use the explicit critical path and the main loop drains after `recv_timeout` wakes it |
| **Bridge** | `runtime.rs` | `spawn_background` bridges Tokio async into the sync TUI, converting errors/panics into notifications |
| **Facade** | `AudioEngineHandle`, `ArtworkLoader`, `PipeWireOutputProvider` | Audio worker (decode, tempo, sink) and picker/probe/decode hidden behind domain handles |
| **Strategy** | `playlist/navigation.rs`, artwork backends | Pure interchangeable selection functions; RatatuiImage, ueberzugpp, ueberzug, or Halfblocks selected by capability and fallback state |
| **State Machine** | `PlaybackMode`, `PlayStatus`, `Popup`, `Panel` | Explicit transitions via enums; TypeState deliberately not forced |
| **RAII / Guard** | `TerminalGuard`, `AppServices::Drop` | Terminal restored even on panic; 5s shutdown grace |
| **Producer-Consumer** | mpsc channels (event bus, `AudioCommand`) | Workers produce, event loop consumes with drain pattern |
| **Pipes and Filters** | scanner, metadata batch, artwork source chain | Iterator pipelines; ordered embedded → cover → folder → front resolution |
| **Chain of Responsibility** | Input routing (dialog → global → panel), artwork and lyrics source order | First match wins; ordered fallback with graceful degradation |
| **Builder** | ratatui widget construction, `default_keymap_summary(&KeysConfig)` | Fluent construction of complex widgets; grouped keymap sections |
| **Newtype** | `ArtworkProtocol` | Bus payload wrapper with its own `Debug` (avoids leaking encoded buffers) |
| **Repository/Store** | `PlaylistStore` (.m3u8) | Persistence decoupled from domain in `data_dir/playlists/` |
| **Dependency Injection** | `AppServices` in `App::execute_effects`, `&Theme` through render | Enables swaps (TOML themes implemented in Phase 7) |
| **Model-View Separation** | `AppState` + immutable `PanelViewModel` + drawing-oriented `draw_*` functions | `App::tick_frame` prepares frame state and `ui::render` remains draw-only |

## Strategies

### Artwork ownership

`ArtworkBackend` owns renderer selection and external-process lifecycle. Its
optional `ArtworkLoader` is shared through `AppServices` for background source
resolution, decoding, and bounded resize work. `AppState::artwork` owns the
current track identity, source policy, loading flag, render protocol, fallback
protocol, and resize responses. `PanelViewModel` projects only immutable artwork
metadata for panel layout; the final native protocol draw is the only render-time
mutable access. External artwork reconciliation happens after the Ratatui frame
draw so backend transitions do not run inside panel rendering.

### Artwork backends and fallback lifecycle

```
auto mode:
  native ratatui-image protocol → RatatuiImage
    └── native resize failure → ueberzugpp → legacy ueberzug → Halfblocks
  halfblocks detection or no picker → ueberzugpp → legacy ueberzug → Halfblocks

image mode:
  native ratatui-image protocol → RatatuiImage
  halfblocks or failed detection → Disabled

unicode mode:
  Picker::halfblocks() → Halfblocks

off mode:
  no picker or artwork pipeline → Disabled
```

`RatatuiImage` is the native `ratatui-image` renderer. In automatic mode,
`ueberzugpp` is the preferred external layer when native detection yields
halfblocks; the original `ueberzug` process is the next fallback. If the
external layer is missing or becomes unhealthy, the artwork state switches to
native Unicode `Halfblocks` and removes any materialized external image.

### Graceful degradation

Every external dependency follows the same pattern:

```
Operation fails
    ↓
tracing::warn!(details)
    ↓
Continue with degraded behavior (no artwork, default config, etc.)
```

This applies to: artwork loading, config parsing, theme loading,
terminal capability detection, metadata extraction, directory scanning, and
stream provider failures. Stream acquisition is detached from the audio worker
and uses cooperative cancellation, bounded provider operations, and request
identity checks so stale results are discarded without blocking command
handling.
The contract is not an unconditional promise that playback never stops:
sink loss currently marks the audio sink as lost and freezes further pumping
without automatic recovery. These are current operational limitations, not
reasons to treat ordinary external failures as process-fatal.

### Configuration and persisted state

- `AppConfig::load` sanitizes persisted numeric values before use. `save`
  rewrites only Harmonium-owned top-level sections while preserving comments
  and unknown user content.
- `PersistedState` is stored in `data_dir/state.toml`. Startup migrates the
  legacy preference fields from that file into `config.toml` and saves the
  normalized configuration.
- `PlaylistStore` shares the same `Paths::data_dir` root: named playlists live
  under `data_dir/playlists/`, while session state lives beside that directory
  in `state.toml`.
- Only a named active playlist is autosaved. Anonymous queue edits do not
  emit the background playlist-save effect.

### Staleness gates

Background completions first pass an `operation_id` gate. Artwork and lyrics
then use track identity, while stream acquisition uses URL and playback-
generation identity checks:

```
Artwork or lyrics event arrives with a track identity
  if the operation is current and the track identity matches → apply
  else → ignore (stale event from a previous track)

Stream source event arrives with URL and generation
  if the operation is current and both match the active request → apply
  else → ignore (stale event from a previous acquisition)
```

This prevents race conditions between the audio worker and the UI
without sharing mutable state across threads. Add Stream additionally cancels
the previous cooperative token on Esc, replacement, or a new submission, and
the UI still rejects any late result whose request identity is no longer
active.

### Startup output replay

Startup enumerates the available PipeWire outputs and matches the persisted
stable sink identifier. An unavailable saved sink falls back to the session
default for the current run without erasing the saved preference. The selected
node is replayed through the audio worker's `SetOutput` command, while Settings
keeps the provider enumeration and live routing responsibilities separate.
