# Technical Implementation

Deep dives into key technical implementations, gotchas, and design decisions.

## Event loop and drain pattern

The main event loop in `src/main.rs` is event-driven and renders only dirty
frames. `TerminalReader` owns the blocking crossterm read and publishes key and
resize events to the shared bounded `EventBus`; the UI thread waits on that same
bus when no frame is dirty.

```text
let _terminal_reader = TerminalReader::spawn(services.events().sender())?;
let mut dirty = true;
let mut pending_event = None;
loop {
    if !dirty {
        match services.events().recv_timeout(POLL_TIMEOUT) {
            Ok(event) => pending_event = Some(event),
            Err(Timeout) => dirty = true,
            Err(Disconnected) => break,
        }
    }

    while let Some(ev) = pending_event.take()
        .or_else(|| services.events().try_recv().ok())
    {
        dirty = true;
        match ev {
            AppEvent::Key(key) => {
                let effects = app.handle_key_event(key);
                app.execute_effects(effects, &services);
            }
            AppEvent::Resize(_, _) | AppEvent::Tick => {}
            AppEvent::PlaybackProgress { snapshot } => {
                app.apply_playback_progress(snapshot);
            }
            AppEvent::TrackEnded { track_index } => {
                let effects = app.apply_track_ended(track_index);
                app.execute_effects(effects, &services);
            }
            AppEvent::CrossfadeCompleted {
                track_index,
                path,
                elapsed,
            } => {
                let effects = app.apply_crossfade_completed(track_index, path, elapsed);
                app.execute_effects(effects, &services);
            }
            AppEvent::StreamResolved { operation_id, .. }
            | AppEvent::ScanCompleted { operation_id, .. }
            | AppEvent::MetadataCompleted { operation_id, .. }
            | AppEvent::ArtworkLoaded { operation_id, .. }
            | AppEvent::LyricsLoaded { operation_id, .. } => {
                if services.accepts_operation_completion(operation_id) {
                    // The concrete apply_* call is selected by the event arm.
                    // Each arm preserves its request, track, or visit gate.
                }
            }
            AppEvent::SourceReady { url, generation } => {
                if let Some(generation) = generation {
                    app.apply_source_ready_with_generation(generation, url);
                } else {
                    app.apply_source_ready(url);
                }
            }
            AppEvent::SourceFailed { url, generation } => {
                if let Some(generation) = generation {
                    app.apply_source_failed_with_generation(generation, url);
                } else {
                    app.apply_source_failed(url);
                }
            }
            AppEvent::Notification { operation_id, message, .. } => {
                if accepts_operation_notification(&services, operation_id) {
                    app.push_notification(message);
                }
            }
        }
    }

    if dirty {
        app.apply_artwork_resizes();
        let area = terminal.terminal_mut().size()?;
        let mut metrics = ui::frame_metrics(area.into());
        metrics.artwork_target = ui::artwork_resize_target(area.into(), app);
        app.tick_frame(metrics, Instant::now());
        terminal.terminal_mut().draw(|frame| ui::render(frame, app, &theme))?;
        dirty = false;
    }
}
```

The drain loop processes **all** currently pending events before the next draw,
reducing event latency while keeping render work bounded to dirty frames.
`EventBus` uses a bounded queue: `Tick`, resize, and periodic playback-progress
events coalesce, while completions, notifications, and state transitions use
the critical path. The regular `send` path returns `Full` when only critical
events occupy the queue. `TerminalReader` uses a bounded critical timeout for
keys and the regular path for resize events, stopping with a warning if the bus
is unavailable. The critical `send_critical` path waits and retries
until the receiver drains capacity or closes, reporting a closed receiver
explicitly.

`StreamResolved` is published through `send_critical`, as are the other
completion, notification, error, and playback state-transition events.
Periodic `PlaybackProgress` uses the regular coalescing path, so a full queue
cannot block the audio worker's command processing. Terminal keys use a bounded
critical send in `TerminalReader`; resize events use the regular path.

**Typed effect boundary**: `AppEvent::Notification` carries a `kind:
EffectErrorKind` (`Audio`, `Task`, or `Playlist`) alongside the message.
Every `spawn_background` call declares the kind its failures belong to, so
the UI can differentiate error sources without parsing strings.

Background completions also carry an `operation_id` registered by
`AppServices`. The main loop releases each operation event after the arm has
accepted or rejected it; completion handlers then apply their narrower
identity: request IDs for browser, playlist, config, and theme work, visit IDs
for theme navigation, queue track identity for artwork and lyrics, and URL plus
playback generation for stream-source boundaries. These gates are independent:
an event can be a current operation while still being stale for the visible
track or request.

## Audio engine worker

The audio worker runs on a dedicated thread that owns decoding (rodio /
Symphonia), the SoundTouch tempo processor, and the native PipeWire output
sink (which drives its own real-time thread):

```text
UI effect/command
  → AudioEngineHandle
  → Sender<AudioCommand>
  → dedicated Worker thread
      → local Decoder, or detached stream acquisition → StreamReader → Decoder
      → direct samples, or SoundTouch for non-default tempo
      → PipeWireSink::push_samples → bounded preallocated sample pool
  → PipeWire stream thread
      → RT process callback → negotiated F32LE device buffer
```

The `Worker` owns decoder state, source-frame accounting, SoundTouch lifecycle,
crossfade state, sink creation, and command ordering. SoundTouch may consume
complete source frames before producing output, so the worker tracks input
credits separately and drains the processor tail before declaring the track
ended. `PipeWireSink` keeps sample descriptors in a bounded pool. Its process
callback drains control commands, flushes old descriptors before acknowledging a
segment boundary, fills the negotiated buffer without allocation, applies the
atomic gain, and updates the played-frame counter. Device changes rebuild the
sink on the worker thread; reconnecting from the PipeWire process callback is
unsafe and can segfault PipeWire.

Commands (`AudioCommand` in `src/audio/engine.rs`): `Play`, `Pause`,
`Resume`, `SetVolume`, `SetGain`, `SetSpeed`, `SetCrossfade`,
`PreloadNext`, `CancelPreload`, `SeekTo`, `Stop`, `SetOutput`, `Shutdown`.

`SetOutput(Option<u32>)` retargets the live stream to a PipeWire sink node
(`None` = session default). It rebuilds the stream instead of
disconnecting/reconnecting from inside the process callback — reconnecting
there segfaults PipeWire — and keeps the decoded source, so playback does
not restart. A failed device switch reports the error and keeps the working
stream.

### Output selection persistence

The selected sink identifier is persisted in `config.toml` under `[sound]` and
startup replays it through `AudioCommand::SetOutput`. If the saved sink is no
longer available, startup falls back to the session/default output while
retaining the saved identifier in Settings so a later reconnect can restore it.
At startup, `select_startup_output` matches the persisted stable sink ID
against the currently available outputs. An unavailable saved sink selects the
session default for this run while leaving the saved ID intact in Settings.
The selected node is then replayed through the worker-owned `SetOutput` command;
settings validation and live routing therefore remain separate operations.

### Stream acquisition and crossfade limits

Initial stream reader and decoder acquisition runs on a detached worker with a
cancellation token and request identity. Stop, replacement, and shutdown do
not join a provider call, and late results are discarded. Providers use
cooperative limits: HTTP/HLS bodies are bounded, yt-dlp has bounded stdout and
stderr plus a wall-clock timeout, and HLS materialization uses the shared
reader cap. Stream crossfade preload is rejected before any provider
`open_reader` call because the current crossfade mixer requires a locally
decoded source; local-file crossfade remains enabled. Live HLS uses bounded
synchronous reads, but the shared cancellation token is checked during HTTP
waits, response reads, and manifest refresh waits. A superseded acquisition
therefore exits cooperatively instead of holding the audio worker until an
unbounded provider call returns. URL and playback-generation checks still
prevent a late result from changing the active track.

The UI applies `SourceReady` and `SourceFailed` through the same URL gate when
older callers do not provide a generation, or through the URL-plus-generation
gate for current audio acquisitions. A matching result clears the stream
loading marker; a stale result is ignored and cannot clear a newer request.

The worker sends back:
- `AppEvent::PlaybackProgress` every ~250ms with a snapshot of status,
  track index, elapsed position, and duration
- `AppEvent::TrackEnded` when the sink drains naturally
- `AppEvent::CrossfadeCompleted` when a crossfade hands over to the
  preloaded next track
- `AppEvent::Notification` (with `EffectErrorKind::Audio`) when the output
  stream is lost (device removed, PipeWire stopped)

**Gotcha**: `AudioCommand::Play` echoes the track label to the UI immediately.
Queue mutations re-anchor the playing state and persistence identity by
`TrackLocation`, and completion handlers can remap an old event index back to
that identity before applying the staleness check. A completion for a removed
track remains a no-op; do not replace the identity gate with index-only logic.

## Artwork pipeline

### Architecture

```
Play command or TrackEnded
  ↓
begin_current_track() → Effect::LoadArtwork { path, track_index }
  ↓
execute_effects → spawn_background("artwork-load")
  ├── spawn_blocking: lofty probe (embedded pictures, local tracks)
  ├── spawn_blocking: cover file search (parent dir, local tracks)
  ├── (when enabled) reqwest fetch from MusicBrainz / Cover Art Archive
  ├── image::load_from_memory / ImageReader::open
  └── picker.new_resize_protocol(img) → ArtworkProtocol
  ↓
AppEvent::ArtworkLoaded { track_index, operation_id, artwork: Option<Box<ArtworkProtocol>> }
  ↓
apply_artwork_loaded → operation and track identity gates → store in ArtworkState
  ↓
apply_artwork_resizes → App::tick_frame → ui::render →
    ArtworkState::render → native or fallback protocol
```

For local tracks the source order is embedded tags, ordered local cover
candidates, then remote lookup. Streams have no local path, so they skip the
first two probes and may use remote lookup from resolved metadata. The bounded
shared resize worker handles protocol encoding away from rendering. The overlay
scales the cover keeping its real aspect ratio, anchored to the panel that is
NOT focused (see the layout section).

The `[ui] artwork_source` config (`all`, `metadata`, `local`, `remote`) selects
which of these sources are consulted.

### Picker construction

`Picker::from_query_stdio()` queries the terminal's escape responses to
detect graphics protocol support and font size. It **must** be called:

1. **After** entering the alternate screen (the query needs the alternate buffer)
2. **Before** the event loop starts reading terminal events (the query reads stdin)

In `src/main.rs`, this is satisfied by constructing the picker inside the
`TerminalGuard` scope, before the splash and before `event_loop()` starts its
`TerminalReader`.

### Send safety

`StatefulProtocol` is verified `Send` at compile time with a const assertion
in `src/artwork/mod.rs`. This allows the picker (wrapped in `Arc<Picker>`) to
be shared with background workers that build protocols off the event loop.

### RefCell for render access

`ArtworkState::render()` needs temporary mutable access to the private
`StatefulProtocol`, but the render path holds `&App`. The protocol is stored
behind `RefCell` inside `ArtworkState`, and the artwork module keeps the
`ratatui-image` widget type behind that facade. This is sound because:

- Rendering is single-threaded
- The borrow lasts exactly one widget draw call
- The internal borrow is never held across frame boundaries

### ratatui-image chafa-dyn pitfall

The default features of `ratatui-image` 11.0.6 pull in `chafa-dyn`, which
build-links against a system `libchafa.so`. This library is not present on
many Linux installations. The fix:

```toml
ratatui-image = { version = "11.0.6", default-features = false, features = ["crossterm"] }
```

### image mode vs auto mode

`Picker::from_query_stdio()` silently falls back to halfblocks when the
terminal query fails. For `album_art_mode = "image"`, this fallback is
unwanted — the user explicitly asked for real graphics only. The fix:
check `picker.protocol_type() != ProtocolType::Halfblocks` after a
successful query.

## Popup and dialog system

### State model

```rust
pub enum Popup {
    ConfirmQuit,
    Help { scroll: u16 },
    PlaylistManager { cursor: usize, names: Vec<String> },
    ConfirmOverwrite { name: String },
    Settings { tab: SettingsTab, focus: SettingsFocus, draft: SettingsDraft },
}

// In AppState:
pub active_popup: Option<Popup>,
pub dialog_mode: Option<DialogMode>, // text-input dialog layered on top
```

Exactly one popup can be active; the naming dialog (`DialogMode`, used by
"Save as", rename, and settings text fields) can be layered on top of the
manager or the settings screen.

### Input routing

When `active_popup` is `Some`, `dialog_command` routes based on the
popup kind:

- **ConfirmQuit**: `y`/`Y` → confirm, `n`/`N`/`Esc` → cancel
- **Help**: `j`/`k`/arrows scroll, `Esc`/`Enter`/`q`/`Ctrl+H` close
- **PlaylistManager**: `j`/`k`/arrows move, `Enter`/`l` load, `d` delete,
  `r` rename, `Esc`/`p` close
- **ConfirmOverwrite**: `Enter`/`y` confirm, `Esc`/`n`/`c` cancel
- **Settings**: `Tab`/`Shift+Tab` switch tabs, `h`/`l`/arrows move between
  columns, `Enter` applies, `Esc` applies and closes (hardcoded, so the
  screen always closes even if `cancel` is rebound)

All other keys are **swallowed** — the UI behind the popup stays frozen.

### Rendering

Popups are drawn **last** in `ui::render`, after all panels. They use
`Clear` to wipe underlying cells, then render a centered bordered block.
The exception is Settings, which renders full-window and hides everything
underneath (the event loop additionally clears the terminal once on the
open/close transition to avoid stale cells).

The centered rectangle is computed by `centered_rect(width, height, area)`
which clamps to fit the terminal size.

## Help popup

### Single source of truth

The help popup content is generated from `default_keymap_summary(&KeysConfig)`
in `src/input.rs`. This function returns eight grouped sections with
`(keys, description)` rows, reflecting the user's configured global
bindings. The footer hints are a separate, smaller list but stay worded
like the summary so the texts never drift apart.

### Invariant

`help_line_count()` and `build_help_lines()` must stay synchronized.
A test in `src/ui/panels/mod.rs` pins this equality by rendering the lines
and comparing the count. If a new section or row is added to
`default_keymap_summary()`, both functions must be updated together.

## Layout engine

### Fixed vertical bands

```
Terminal area
  ├── main (browser + playlist)  Min(2), takes leftover space
  ├── now_playing                Length(5)  [shown when height ≥ 21]
  └── footer                     Length(1)
```

There is no standalone status band: the status line is rendered inside the
Now Playing block. The main split is horizontal — browser `Ratio(1, 3)`,
a one-cell gap (`PANEL_GAP`), playlist `Ratio(2, 3)`. Album artwork is not
a band: it is a focus-aware overlay painted on top of the unfocused panel.

### Compact threshold

When the terminal height is below `COMPACT_HEIGHT_THRESHOLD` (21 rows),
the Now Playing band collapses to hidden and the main area gets
the full height. This prevents the layout from becoming unusable on
small terminals.

### Layout tests

`src/ui/layout.rs` has unit tests that verify exact `Rect` coordinates
for specific terminal sizes (e.g., 90×30, 60×30, the threshold height
itself). These tests catch regressions in the layout math.

## XDG paths and persistence

### Directory structure

```
~/.config/harmonium/          config.toml, themes/
~/.local/share/harmonium/     playlists/ (M3U files), state.toml (session state)
~/.cache/harmonium/           harmonium.log, temporary data
```

### Paths resolution

`Paths::system()` uses the `directories` crate with
`ProjectDirs::from("", "", "harmonium")`, falling back to `$HOME` layout
when the XDG base directories are unavailable.

### M3U playlist persistence

`PlaylistStore` reads and writes `.m3u8` files in `data_dir/playlists/`.
Saving writes an extended header: one `#EXTINF:<seconds>,<artist - title>`
line before each path (with the `-1` duration sentinel and a file-name
display fallback when metadata is missing). Line-breaking controls in labels
are escaped so an EXTINF label cannot inject another M3U entry; ordinary
labels are unchanged. Local identities use the lossless
`harmonium-local-v2:<hex-bytes>` body encoding, while the ambiguous legacy
`harmonium-local-v1:` marker is preserved as an opaque local-path prefix to
avoid silently truncating a literal path. Parsing takes the `#EXTINF` label as
a display fallback until real metadata arrives, resolves relative paths
lexically against the playlist directory without canonicalization, and queues
`http(s)://` entries as stream tracks. Direct additions and browser actions
anchor relative paths lexically at the process working directory before queue
identity matching. Stream classification routes generic HTTP URLs through the
generic HTTP provider, while Radio Browser metadata enrichment is attempted
only for Radio Browser hosts.

## Testing approach

### Philosophy

Tests exist where they add value: pure domain logic, boundary conditions,
and integration contracts. Artificial tests that merely increase coverage
are avoided.

### Categories

| Category | What's tested | Example |
|----------|---------------|---------|
| Domain logic | Pure functions, state transitions | `PlaybackMode` six-combination coverage |
| Boundary conditions | Edge cases, empty inputs, caps | `push_notification` with zero cap |
| Input routing | Key → Command mapping | Dialog swallows global bindings |
| Layout | Exact Rect coordinates | 90×30 terminal produces correct bands |
| Config | Parse valid/invalid/partial TOML | Unknown mode value → defaults |
| Integration | EventBus round-trip, staleness gates | Artwork loaded for wrong track → ignored |

### Test fixtures

- `src/artwork/testing::tiny_png()` — smallest valid PNG for decode tests
- `src/artwork/testing::test_protocol()` — real protocol over tiny image
- `src/test_support::{unique_temp_dir, TestTempDir}` — RAII temp directory lifecycle
- `TestBackend` from ratatui — render tests without a real terminal

### Running tests

```bash
cargo test                    # all tests
cargo test -- --nocapture     # with stdout
cargo test artwork            # filter by name
```
