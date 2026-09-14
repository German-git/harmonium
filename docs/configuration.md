# Configuration

Configuration file reference, XDG directories, and runtime options for Harmonium.

## Config file location

```
~/.config/harmonium/config.toml
```

Harmonium reads the file when present and otherwise uses built-in defaults.
A missing file never blocks startup; an invalid one logs a warning and falls
back to defaults. Note that the player rewrites only its owned sections
(`[ui]`, `[general]`, `[keys]`, `[playback]`, `[sound]`, `[log]`) when it
saves — at startup after applying preferences, and after a settings edit —
preserving comments and unknown sections verbatim.
(Theme files under `themes/` are also written on first run; see Theme files.)

## Loading behavior

| Scenario | Behavior |
|----------|----------|
| File doesn't exist | Use defaults, debug log |
| Valid TOML, unknown fields | Ignore unknown fields, use recognized values |
| Invalid TOML syntax | `tracing::warn` with parse error, use defaults |
| Missing section (e.g., `[ui]`) | Use defaults for that section |
| Unknown value for enum (e.g., `album_art_mode = "banana"`) | Treated as a parse failure: `tracing::warn`, the **entire file** falls back to defaults |

## Sections

### `[general]`

```toml
[general]
confirm_quit = true
volume_percent = 70
resume_previous_track = false
artwork_visible = true
show_hidden = false
```

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `confirm_quit` | bool | `true` | Whether `q` opens a confirmation popup or quits immediately |
| `volume_percent` | u16 | `70` | Volume level 0–100; values above 100 are clamped on load |
| `resume_previous_track` | bool | `false` | Restore and auto-play the last track at its saved position on startup |
| `browser_directory` | string | `""` | Last opened browser directory (kept in sync automatically) |
| `artwork_visible` | bool | `true` | Whether the artwork overlay is shown (the `z` toggle writes here) |
| `show_hidden` | bool | `false` | Whether dotfiles appear in the file browser |

#### `[general.playlist_columns]` and `[general.now_playing_display]`

The tables have related but distinct roles: `playlist_columns` controls queue
labels and metadata ordering, while `now_playing_display` controls which fields
the Now Playing band shows. In playlist-column metadata mode, title is always
included; the other metadata fields are opt-in.

```toml
[general.playlist_columns]
display_by = "filename"       # "filename" | "metadata"
metadata_track_number = false
metadata_artist = false
metadata_album = false

[general.now_playing_display]
sort_by = "filename"
metadata_track_number = false
metadata_artist = false
metadata_album = false
metadata_title = false
```

With `display_by = "metadata"`, title plus the enabled fields take part in
playlist labels and ordering. With `sort_by = "metadata"` in
`now_playing_display`, the enabled fields take part in the display. A track
without usable metadata falls back to its file name.

Older files may still contain `[general.sort_tracks]`. Harmonium reads that
legacy table when `[general.playlist_columns]` is absent and writes the current
`playlist_columns` shape when it saves the configuration.

### `[ui]`

```toml
[ui]
show_album_art = true
album_art_mode = "auto"
artwork_source = "all"
theme = "default"
border_type = "plain"
```

| Key | Type | Default | Values | Description |
|-----|------|---------|--------|-------------|
| `show_album_art` | bool | `true` | `true` / `false` | Enable or disable the album artwork pipeline entirely |
| `album_art_mode` | string | `"auto"` | `"auto"`, `"image"`, `"unicode"`, `"off"` | Which terminal protocol to use for artwork display |
| `artwork_source` | string | `"all"` | `"all"`, `"metadata"`, `"local"`, `"remote"` | Which cover-art sources to consult: all, embedded tags only, local files only, or remote (MusicBrainz) only |
| `theme` | string | `"default"` | any theme name | Name of the theme file to load from `themes/` |
| `border_type` | string | `"plain"` | `"plain"`, `"rounded"`, `"double"`, `"thick"` | Border glyph style used by bordered UI elements |

**`album_art_mode` values:**

| Value | Behavior |
|-------|----------|
| `auto` | Query terminal capabilities via escape sequences; use the best available protocol (Kitty/Sixel/iTerm2); fall back to Unicode halfblocks if no protocol detected |
| `image` | Require a real graphics protocol; if the terminal only supports halfblocks or nothing, artwork is disabled with a warning |
| `unicode` | Force Unicode half-blocks; never query the terminal (safe for VTE-based terminals like XFCE4 Terminal, Guake) |
| `off` | Disable artwork entirely; no image loading, no protocol detection |

### `[keys]`

Global playback and UI keybindings. Only these twelve global actions are
configurable today; panel-specific keys (browser navigation, playlist
editing) remain hardcoded.

```toml
[keys]
quit = "q"
help = "ctrl+h"
play_pause = "space"
next = "n"
previous = "N"
volume_up = "+"
volume_down = "-"
repeat = "m"
shuffle = "s"
cancel = "esc"
toggle_artwork = "z"
lyrics = "shift+L"
```

`cancel` and `toggle_artwork` are additional shortcuts: `Esc` always closes
or cancels the active popup regardless of this binding, and `Shift+C`
always opens the settings editor. The `lyrics` binding toggles the lyrics
panel, with the uppercase `L` key kept as a hardcoded fallback.

**Key string format**: lowercase key name with `+`-separated modifiers.
Examples: `"q"`, `"ctrl+h"`, `"shift+tab"`, `"space"`, `"esc"`,
`"enter"`, `"pageup"`, `"g"`, `"["`.

Unknown or invalid key strings fall back to the built-in default for
that action and log a warning.

### `[playback]`

```toml
[playback]
remote_lyrics = false
gain_db = 0.0
crossfade_seconds = 0
```

| Key | Type | Default | Valid range | Description |
|-----|------|---------|-------------|-------------|
| `remote_lyrics` | bool | `false` | — | Whether the LRCLIB provider may be consulted for lyrics; disabled means local files and embedded tags only |
| `gain_db` | f32 | `0.0` | −15.0 to +15.0 | Preamp gain in decibels, applied before the master volume; out-of-range values are clamped on load, non-finite values reset to 0 |
| `crossfade_seconds` | u16 | `0` | `0`, or 5/10/15/20/25/30 | Crossfade length between consecutive tracks; `0` disables crossfade. Values above 30 are clamped to 30 on load, and enabled values that are not multiples of 5 are snapped down to the nearest multiple |

### `[sound]`

```toml
[sound]
output_sink_id = ""
```

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `output_sink_id` | string | `""` | Stable identifier of the PipeWire output sink (the settings Sound tab writes it); empty means the session default sink |

### `[log]`

```toml
[log]
level = "info"
```

| Key | Type | Default | Values | Description |
|-----|------|---------|--------|-------------|
| `level` | string | `"info"` | `"trace"`, `"debug"`, `"info"`, `"warn"`, `"error"` | Tracing filter for the file log (`~/.cache/harmonium/harmonium.log`). The `RUST_LOG` environment variable overrides this value. |

## Theme files

Theme files live in:

```
~/.config/harmonium/themes/
├── default.toml                  (bundled on first run)
├── gruvbox.toml                  (bundled on first run)
├── <22 other bundled themes>     (see [themes.md](./themes.md))
└── custom.toml                   (user-created)
```

Each theme defines a `[colors]` section with twenty-four color fields:
nineteen global roles plus five lyrics overrides (Lyrics colors). Keys are
persisted in alphabetical order — the example below is the bundled
`default.toml` verbatim:

```toml
[colors]
artwork_area = "black"
background = "black"
border = "dark_gray"
border_focused = "cyan"
error = "red"
foreground = "white"
highlight = "yellow"
lyrics_background = "black"
lyrics_border = "dark_gray"
lyrics_border_focused = "cyan"
lyrics_highlight = "cyan"
lyrics_text = "dark_gray"
paused = "yellow"
playing = "green"
popup_border = "cyan"
progress = "cyan"
selection = "blue"
separator = "dark_gray"
status_line = "white"
stopped = "dark_gray"
success = "green"
text_muted = "dark_gray"
time_text = "white"
warning = "yellow"
```

An empty string for any field is treated as "unset" and keeps the built-in
default, so a partially edited theme file stays valid. The five `lyrics_*`
keys work the same way, except their fallback is the theme's own role rather
than a fixed default (Lyrics colors).

**Color value formats**:
- Named colors: `"black"`, `"red"`, `"green"`, `"yellow"`, `"blue"`,
  `"magenta"`, `"cyan"`, `"white"`, `"gray"`, `"dark_gray"`, etc.
- Indexed colors: `"0"` through `"255"`
- TrueColor hex: `"#rrggbb"`

### Lyrics colors

The lyrics panel can be restyled independently through five keys. When set,
a key overrides its role inside the lyrics panel only; when unset, the panel
keeps inheriting the effective theme:

| Key | Overrides when set | Inherits when unset |
|-----|--------------------|---------------------|
| `lyrics_text` | Regular lyric text: plain lines, not-yet-started karaoke lines, and the loading / "not found" messages | `foreground` in plain documents, `text_muted` elsewhere |
| `lyrics_highlight` | The current (reached) karaoke line | `highlight` |
| `lyrics_background` | The panel background and border fill | `background` |
| `lyrics_border` | The panel border and title in both focus states unless `lyrics_border_focused` narrows the focused one; when it covers both states, focus stays marked by the bold title and border | `border` (unfocused) and `border_focused` (focused) |
| `lyrics_border_focused` | The panel border and title while the lyrics panel is focused | `lyrics_border` if set, otherwise `border_focused` |

Border resolution while focused follows the chain `lyrics_border_focused` →
`lyrics_border` → `border_focused`; while unfocused it is `lyrics_border` →
`border`. When two overrides leave the focused and unfocused borders on the
same color, focus stays marked by the bold title and bold border.

Both bundled themes define these five keys explicitly, each seeded from
its own palette: `lyrics_text` = `text_muted`, `lyrics_highlight` =
`progress`, `lyrics_background` = `background`, `lyrics_border` = `border`,
`lyrics_border_focused` = `border_focused`. The border pair therefore keeps
the classic focused-accent behavior for the lyrics panel. The
unset-inherit fallback stays for user themes and older files without the
keys. Because unset means "inherit at render time", a gruvbox clone without
lyrics keys still shows gruvbox's own muted and highlight colors, never the
built-in defaults. Saving from the settings theme editor writes the global
roles as resolved values (flattened into the file); the lyrics keys are only
flattened when you actually set them.

Theme files are persisted with alphabetically sorted color keys, while the
settings editor lists the same fields grouped by role — two deliberately
different orders.

**Loading order**: built-in defaults → `themes/default.toml` (if exists) →
user-selected theme from `[ui] theme` → `themes/<name>.toml` merged over
defaults. Missing or invalid theme files degrade with a warning.

On first run, every bundled theme listed in [themes.md](./themes.md) is
written to the themes directory if it doesn't exist. Existing files are never
overwritten.

## XDG directories

Harmonium follows the XDG Base Directory Specification via the `directories` crate:

| Directory | Default path | Purpose |
|-----------|-------------|---------|
| Config | `~/.config/harmonium/` | `config.toml`, `themes/` |
| Data | `~/.local/share/harmonium/` | `playlists/` (M3U files), `state.toml` (session state) |
| Cache | `~/.cache/harmonium/` | `harmonium.log`, temporary/reconstructible data |

If XDG environment variables are set, they are respected. If not, the
crate falls back to `$HOME`-relative paths.

## State persistence

Session state is persisted to `~/.local/share/harmonium/state.toml` on
graceful exit and restored at startup. This keeps `config.toml` user-owned
and avoids losing TOML comments through re-serialization.

Only session-scoped state lives here — user preferences belong in
`config.toml`:

```toml
repeat_mode = "off"
shuffle = false
last_playlist = "favorites"
last_track_path = "harmonium-local-v2:2f686f6d652f757365722f4d757369632f416c62756d2f736f6e672e6d7033"
last_track_position_ms = 42000
```

`last_track_path` is the serialized `TrackLocation`, not always a literal
filesystem path. Local identities use the lossless
`harmonium-local-v2:<lowercase-hex>` format, where the payload is the normalized
path bytes. A stream identity is stored as its `http://` or `https://` URL. The
loader still accepts legacy unencoded local paths, and malformed v2 values are
kept as opaque local-path values rather than being silently truncated. Relative
local identities are resolved against the owning playlist directory or other
explicit boundary; they are not canonicalized or probed during deserialization.

| Key | Type | Description |
|-----|------|-------------|
| `repeat_mode` | string | `"off"`, `"track"`, or `"all"` |
| `shuffle` | bool | Whether shuffle is enabled |
| `last_playlist` | string (optional) | Name of the playlist restored into the queue on launch; absent starts an empty queue |
| `last_track_path` | string (optional) | Serialized `TrackLocation` to resume when `resume_previous_track` is on: lossless `harmonium-local-v2:` encoding for local paths or the canonical stream URL |
| `last_track_position_ms` | u64 | Resume position in milliseconds inside `last_track_path` |

Older versions also stored volume, `confirm_quit`, resume tracking, browser
directory and artwork visibility in `state.toml`. Those preferences now live
in `config.toml` under `[general]`; at startup a legacy value is migrated only
when its corresponding key is absent from the current configuration, so an
explicit user value always wins. The migration writes `config.toml` before it
cleans the legacy keys from `state.toml`. This is intentionally recoverable but
not atomic across the two files: a pre-replacement failure leaves legacy keys
for retry, while a later config edit remains authoritative and is not
overwritten. A post-replacement directory-sync error is checked against the
resulting file before cleanup is reported as incomplete.

Missing or invalid state files silently use defaults.
