# Features

Detailed functionality specification for Harmonium, organized by feature area.

## File browser

The file browser occupies the left third of the terminal. It provides a
tree-like navigation interface starting from `$XDG_MUSIC_DIR` when set,
otherwise `$HOME/Music/`, falling back to `$HOME/`. Hidden files and
dot-directories are not shown by default; the "Show hidden files" option in
the settings editor (General tab) toggles them.

### Navigation

| Key | Action |
|-----|--------|
| `j` / `Down` | Move cursor down |
| `k` / `Up` | Move cursor up |
| `g` / `Home` | Jump to first entry |
| `G` / `End` | Jump to last entry |
| `PageUp` / `PageDown` | Page the view |
| `h` / `Left` | Open parent directory |
| `l` / `Right` / `Enter` | Open directory or add file to playlist |

Directories are expanded in place; regular files are added to the playlist
as a single entry when activated.

### Multi-select

| Key | Action |
|-----|--------|
| `v` | Toggle a mark on the cursor entry |
| `a` | Add all marked entries (or the cursor entry if nothing is marked) |

Marks are cleared on every directory change. This is a documented decision:
a predictable mental model (marks = current directory) beats cross-directory
persistence, while scrolling and paging always preserve them.

## Playlist / queue

The playlist occupies the right two-thirds of the terminal. It is a queue
in user order, not a library. Removing entries never deletes the underlying
files.

### Management

| Key | Action |
|-----|--------|
| `l` / `Enter` | Play the selected entry |
| `j` / `Down` | Move cursor down |
| `k` / `Up` | Move cursor up |
| `J` | Swap selected entry down |
| `K` | Swap selected entry up |
| `x` | Delete the selected queue entry |
| `D` | Clear the entire queue |
| `d` | Delete the active (named) playlist |

### Clear queue behavior

When the queue is cleared (`D`), the currently playing track continues until
its natural end. The completion path sees an empty queue and stops gracefully.
No files are ever deleted.

### Track sorting

The queue can be reordered according to `[general.playlist_columns]`: by file
name (default) or by metadata. The configurable metadata fields are artist,
album, and track number; title is always included in playlist-column metadata
mode. Tracks without usable metadata fall back to the file name. Sorting is
applied when explicitly requested in the settings editor; it never modifies
source files.

### Structural mutation and navigation

Structural queue edits (delete, swap, clear) rebuild the `NavigationState`
wholesale because old indices become stale. Tail appends join the unplayed
pool incrementally without a full rebuild.

## Playlist manager

Named playlists are saved as `.m3u8` files in
`~/.local/share/harmonium/playlists/` and restored on demand. The manager is a
modal popup opened with `p`.

### Keys

| Key | Action |
|-----|--------|
| `p` | Open or close the playlist manager |
| `A` | Create a new playlist |
| `r` | Rename the active or selected playlist |
| `a` | Save the current queue as a playlist (suppressed while the browser panel is focused) |
| `d` | Delete the active or selected playlist |
| `up` / `down` (in popup) | Move the manager cursor |
| `Enter` / `l` (in popup) | Load the selected playlist |
| `Esc` / `p` (in popup) | Close the manager |

Saving the queue persists it as M3U; reloading restores the track order. The
active playlist name is stored in `state.toml` and reloaded on the next launch.
Overwriting an existing name asks for confirmation (`y` / `Enter` to confirm,
`n` / `c` / `Esc` to cancel).

## Playback

Tracks are decoded by Rodio (Symphonia) and rendered through a native
PipeWire output sink. When no PipeWire session is available, opening the
output fails per play attempt with a visible notification — startup and the
rest of the UI keep working.

### Controls

| Key | Action |
|-----|--------|
| `Space` | Toggle pause (does not start from stopped state) |
| `n` | Play the next track |
| `N` | Previous track or restart current track (if past 3 seconds) |
| `+` / `-` | Volume up / down |
| `[` / `]` | Decrease / increase playback speed (0.5x–2.0x, pitch preserved) |
| `\` | Reset playback speed to 1.0x |
| `Left` / `Right` | Seek backward / forward (adaptive step, while the playlist panel is focused) |
| `Esc` | Cancel / close the active popup |

## Streams

Press `Shift+S` to open the Add Stream popup and enter an HTTP(S) URL. Streams
join the same queue as local tracks and are resolved when playback starts.

| Provider | Accepted input | Runtime requirement |
|----------|----------------|---------------------|
| YouTube | `youtube.com`, `www.youtube.com`, `m.youtube.com`, `music.youtube.com`, or `youtu.be` URLs | `yt-dlp` |
| Radio Browser | Supported `radio-browser.info` station pages | Access to the public Radio Browser API |
| Generic HTTP | `http://` or `https://` stream URLs, including raw MP3/AAC/OGG/Opus streams | A reachable endpoint |
| HLS | HTTP responses with `application/vnd.apple.mpegurl` or an `.m3u8` path | A reachable endpoint |

YouTube metadata and direct media URL resolution use `yt-dlp`. Radio Browser
station metadata is queried only for recognized Radio Browser hosts; a station's
resolved stream is then opened through the HTTP path. Provider metadata is
best-effort, so the URL host remains the display fallback when no title is
available.

Stream resolution, provider I/O, and stream decoder setup run outside the UI and
audio worker threads; local decoding remains worker-owned. Selecting another
local track or stream cancels the previous acquisition. Current `SourceReady`
and `SourceFailed` events are accepted only
when their URL and playback generation match the active request; compatibility
events without a generation use the URL gate. Late results cannot replace a
newer selection. Live HLS reads and manifest refresh waits also observe
cancellation.

The current crossfade mixer requires a locally decoded source, so stream
crossfade preload is rejected and local-file crossfade remains available.

## Local audio formats

The file browser recognizes these extensions case-insensitively:
`mp3`, `flac`, `wav`, `aiff`, `aif`, `ogg`, `oga`, `opus`, `m4a`, `m4b`,
`mp4`, and `aac`. The decoder still determines whether an individual file can
be played successfully.

Speed, crossfade and preamp gain are adjusted through the settings editor
(or `config.toml`, see [configuration.md](configuration.md)):

- **Playback speed** uses SoundTouch to change tempo without altering pitch;
  it is not persisted and resets to 1.0x on each launch.
- **Crossfade** mixes the incoming track over the outgoing one; valid
  lengths are 0 (off) or 5–30 seconds in steps of 5.
- **Preamp gain** (−15 to +15 dB) is applied before the master volume.
- **Output device**: the Sound tab lists the PipeWire sinks of the session;
  the selection is stored as `output_sink_id` under `[sound]`.

### Playback state

The `PlaybackState` is updated optimistically by commands (instant visual
feedback) and corrected by `PlaybackProgress` snapshots from the audio
worker.

### Auto-advance

When a track ends (`TrackEnded` event), the selection engine determines
the next track based on the current `PlaybackMode`. The staleness gate
(`track_index != Some(i) => ignore`) prevents stale completion events from
triggering unintended advances.

## Playback modes

The `PlaybackMode` model has two independent axes:

```
RepeatMode: Off | Track | All
shuffle:    bool
```

This yields six combinations:

| Repeat | Shuffle | Behavior |
|--------|---------|----------|
| Off | Off | Play queue in order, stop at end |
| Track | Off | Repeat current track forever |
| All | Off | Play queue in order, loop back to start |
| Off | On | Shuffle once through all tracks, then stop |
| Track | On | Shuffle through all tracks, repeat current on completion |
| All | On | Shuffle through all tracks, loop back to start |

### Shuffle semantics

Shuffle keeps a `Vec` pool of not-yet-played indices with O(1) swap-remove
draws, plus a `Vec` played-history trail. The randomness comes from an
inline `SplitMix64` seeded from the wall clock (no new dependencies). On
pool exhaustion the pass regenerates, temporarily moving the previous draw
aside to avoid an immediate repeat when more than one track exists.

### Keys

| Key | Action |
|-----|--------|
| `m` | Cycle repeat mode: Off → Track → All |
| `s` | Toggle shuffle on/off |

## Album artwork

The artwork is a focus-aware overlay: it is anchored to the panel that is
NOT focused (playlist or browser), so the panel you are working in always
stays readable. While the lyrics panel is visible the cover never overlaps
it. The overlay is drawn after the panels, covering them.

### Display

| Key | Action |
|-----|--------|
| `z` | Toggle artwork visibility (show/hide the overlay) |

The overlay scales the cover with its real aspect ratio inside the target
panel; it cannot be resized manually. Its visibility is persisted across
sessions in `config.toml` under `[general] artwork_visible`.

### Source and capability detection

Harmonium resolves cover art for a track in this order, stopping at the first
hit:

1. **Embedded artwork** (via lofty tags in the audio file; local tracks only)
2. **Local cover files** in the track's parent directory (local tracks only):
   `cover.jpg` → `cover.jpeg` → `cover.png` → `folder.jpg` →
   `folder.jpeg` → `front.jpg` → `front.png`
3. **Remote lookup**: cover fetched from MusicBrainz / Cover Art Archive using
   track metadata; this is also the only artwork source attempted for streams
4. **Visual fallback**: Unicode half-blocks when no graphics protocol is available
5. **No artwork**: text and gauge fill the full width

The `[ui] artwork_source` config (`all`, `metadata`, `local`, `remote`) restricts
which of the first three sources are consulted. Terminal protocol selection
(Kitty/Sixel/iTerm2 vs. halfblocks) follows the `album_art_mode` setting described
below.

### Terminal protocols

The `album_art_mode` config controls which protocol to use:

| Mode | Behavior |
|------|----------|
| `auto` | Query terminal capabilities; fall back to halfblocks |
| `image` | Require real graphics protocol (Kitty/Sixel/iTerm2); disable if unavailable |
| `unicode` | Force Unicode half-blocks, never query terminal |
| `off` | No artwork at all |

### Non-blocking pipeline

Artwork decoding happens **off the event loop** via `spawn_blocking`; protocol
resizing is handled by the bounded shared artwork worker rather than during
rendering. The result is delivered through the `EventBus` as an
`AppEvent::ArtworkLoaded`. A track-index identity gate ensures the artwork
belongs to the currently playing track, and pending resize responses are
applied before the next draw.

## Lyrics

| Key | Action |
|-----|--------|
| `Shift+L` | Toggle the lyrics panel (replaces the browser in the left third) |
| `j` / `k`, `g` / `G`, `PageUp` / `PageDown` | Scroll the lyrics while the panel is focused |

Lyrics are resolved in priority order — local `.lrc` file (recursive search
around the audio file), embedded synced-lyrics tags, then the LRCLIB
provider — and the chain is memoized per track path. Remote lookup is **off
by default**; it is enabled by the "Remote lyrics" checkbox in the settings
Playback tab (persisted as `[playback] remote_lyrics`). Accepted remote
results are cached as `<audio-dir>/lyrics/<stem>.lrc`.

Timed (and enhanced, word-level) LRC documents drive a karaoke follow: the
panel auto-scrolls to keep the active line in view, re-anchoring on seeks
and resizes; manual scrolling takes over between line changes.

The panel's colors are themeable through the `lyrics_text`,
`lyrics_highlight`, `lyrics_background`, `lyrics_border` and
`lyrics_border_focused` keys; unset keys keep the current behavior of
inheriting the theme's text, highlight, background and border roles (see
[configuration.md](configuration.md)).

## Settings editor

| Key | Action |
|-----|--------|
| `Shift+C` | Open the full-window settings editor (hardcoded, always reachable) |
| `Tab` / `Shift+Tab` | Switch between the five tabs |
| `h` / `l` or `Left` / `Right` | Move between the columns of the active tab |
| `j` / `k` or `Up` / `Down` | Move the row cursor |
| `Enter` | Apply the focused field (or apply all and close) |
| `Esc` | Apply changes and close (cancels an in-progress text edit first) |

The editor has five tabs: **General** (browser directory, resume track,
confirm quit, show hidden files, sort tracks), **Appearance** (theme
selection, color editing), **Keys** (binding editor), **Playback** (remote
lyrics, gain, crossfade), and **Sound** (output device selection). Edits
work on a draft copy and are applied and persisted to `config.toml` when
the popup closes.

## Help popup

| Key | Action |
|-----|--------|
| `Ctrl+H` | Toggle help popup |
| `?` | Toggle help popup (same action) |

The popup is a scrollable table grouped into eight sections (Global,
Navigation, File Browser, Playlist, Streaming, Playlist Manager, Popups,
Settings).
It is fed by `default_keymap_summary(&KeysConfig)`, the single source of
truth for all keybindings, which reflects the user's configured global
keys.

### Scroll controls

| Key | Action |
|-----|--------|
| `j` / `k` or `Down`/`Up` | Scroll by one line |
| `PageUp` / `PageDown` | Scroll by page |
| `g` / `Home` | Jump to top |
| `G` / `End` | Jump to bottom |
| `Esc`, `Enter`, `q`, `Ctrl+H` | Close the popup |

## Notifications

Notifications are transient messages stored in a bounded buffer (capacity
50). They are surfaced in the status area, e.g. when artwork, config, or a
background scan fails and degrades gracefully.

## Quit confirmation

When `confirm_quit` is enabled (default: `true`), pressing `q` opens a
confirmation popup:

| Key | Action |
|-----|--------|
| `y` / `Y` | Confirm quit |
| `n` / `N` / `Esc` | Cancel |

The popup swallows all other keys to prevent accidental actions while it
is visible.

## Startup splash

Before Ratatui attaches, a 600 ms themed scanline animation reveals the
`harmonium` logo. `Esc` or `q` skips it; other key events are forwarded to the
shared event bus instead of being discarded. Worker events remain queued while
the splash is active. The splash never blocks boot — a failure degrades to a
log warning. See [terminal-splash.md](terminal-splash.md) for the design and
rationale.
