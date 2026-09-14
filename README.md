# Harmonium

A local music player running entirely inside the Linux terminal.

Harmonium is a terminal user interface (TUI) application built in Rust that provides
a complete music playback experience from the command line. Inspired by
[Termusic](https://github.com/tramhao/termusic) but with an original implementation,
it supports audio playback, file browsing, playlist management, album artwork display,
and configurable keybindings — all without leaving the terminal.

## Status

The current source includes the documented playback, stream, lyrics, settings,
artwork, persistence, and hardening work. The supported artifact is still a
locally built Linux/PipeWire binary: packaging, signing, update channels, and a
first release have not been provided.

## Features

| Area | What it does |
|------|--------------|
| **File browser** | Navigate directories, multi-select tracks, add individual files or entire folders to the playlist; hidden files optional (off by default) |
| **Playlist/queue** | Add, remove, reorder, clear tracks; play any entry; never deletes source files |
| **Playlist manager** | Create, rename, delete, and save/load named playlists persisted as M3U files |
| **Playback** | Play, pause, next, previous, seek, volume control — Rodio/Symphonia decoding into a native PipeWire output sink |
| **Repeat & shuffle** | Two-axis `PlaybackMode` model: repeat (Off/Track/All) × shuffle, six combinations, avoids accidental repeats |
| **Playback speed** | `[` / `]` adjust tempo between 0.5x and 2.0x (pitch preserved via SoundTouch), `\` resets to 1.0x |
| **Crossfade & gain** | Optional crossfade between consecutive tracks (0 or 5–30 s) and preamp gain (−15 to +15 dB), configured in Settings |
| **Output device** | Select the PipeWire sink in the Settings Sound tab; empty choice follows the session default |
| **Track sorting** | Reorder the queue by file name or by metadata fields (artist / album / track / title), configured in Settings |
| **Album artwork** | Focus-aware overlay anchored to the unfocused panel, toggle visibility with `z`; sources: embedded tags, local cover files, remote lookup (MusicBrainz); capability-tolerant (Kitty/Sixel/iTerm2/Unicode halfblocks) |
| **Lyrics** | `Shift+L` toggles a lyrics panel replacing the browser; sources: local `.lrc` files (recursive search), embedded tags, LRCLIB remote lookup (**off by default**, enable in Settings); timed and enhanced LRC supported with karaoke auto-scroll |
| **Settings editor** | Full-window editor with five tabs (General, Appearance, Keys, Playback, Sound), opened with `Shift+C` |
| **Startup splash** | Brief 600 ms themed scanline animation before the UI attaches; `Esc` or `q` skips it, while other key events are forwarded to the event bus |
| **Help popup** | `Ctrl+H` or `?` opens a scrollable keybinding reference grouped by context |
| **Configurable keybindings** | Global keys remappable through `config.toml` `[keys]` section |
| **Themes** | TOML theme files in `~/.config/harmonium/themes/` (24 bundled palettes, including `default` and `gruvbox`) |
| **Graceful degradation** | Missing artwork, corrupt config, unsupported terminal capabilities, unavailable audio output — all degrade with warnings, never block |

## Quick start

### Prerequisites

- Rust 1.98.0 (edition 2024, the tested toolchain)
- PipeWire development packages (`libpipewire` and SPA headers, discovered via `pkg-config`) for the native audio output
- A C++ toolchain (e.g. `g++` or `clang`) to build the bundled SoundTouch FFI bridge used for playback speed
- ALSA development libraries (required by the default `cpal` backend)
- A terminal with at least Unicode support (Kitty, iTerm2, Sixel, or VTE-based terminals all work)

### Supported build profile

The only supported build profile is the current Linux/PipeWire profile. The
repository does not currently prove a supported Windows, macOS, alternate audio
backend, or feature-disabled profile. Native integrations are always compiled:
PipeWire/libspa provides audio output, `cpal` and ALSA support output discovery,
SoundTouch provides pitch-preserving playback speed, and the image/terminal
adapters provide the artwork fallback chain. There are no Cargo features that
select another supported combination.

Do not treat `--all-features`, `--no-default-features`, or a guessed feature such
as `pipewire` or `artwork-external` as release profiles. `--all-features` is
only a validation command here, and the other forms do not describe supported
builds. Optional runtime tools such as `yt-dlp`, `ueberzugpp`, `ueberzug`, and
`xwininfo` improve specific capabilities but do not create alternate Cargo
profiles. See [docs/development.md](docs/development.md) for the support matrix
and release checklist.

### Build and run

```bash
cargo build --release
cargo run --release
```

The application starts in `$XDG_MUSIC_DIR` when set, otherwise
`$HOME/Music/`, falling back to `$HOME/`.
Navigate with the keyboard, add tracks to the playlist, and press `Space` to play.

## Default keybindings

| Key | Action |
|-----|--------|
| `q` | Quit (asks for confirmation) |
| `Space` | Toggle pause |
| `n` / `N` | Next track / Previous track (or restart current) |
| `+` / `-` | Volume up / down |
| `m` | Cycle repeat mode (Off → Track → All) |
| `s` | Toggle shuffle |
| `[` / `]` | Playback speed down / up (0.5x–2.0x) |
| `\` | Reset playback speed to 1.0x |
| `Ctrl+H` or `?` | Open/close help popup |
| `Shift+C` | Open settings editor (closes with `Esc`, applying changes) |
| `Esc` | Cancel / close the active popup |
| `z` | Toggle album artwork visibility |
| `Shift+L` | Toggle the lyrics panel |
| `Shift+S` | Open the Add Stream popup |
| `p` | Open/close playlist manager |
| `Tab` / `Shift+Tab` | Switch panel focus |
| `j`/`k` or arrows | Move cursor in focused panel |
| `v` | Toggle mark on browser entry (multi-select) |
| `a` | Add marked entries (or cursor entry) to playlist |
| `h`/`l` or arrows | Navigate directories (browser) |
| `l`/`Enter` | Play selected entry (playlist) |
| `J`/`K` | Swap playlist entry down/up |
| `x` | Delete selected queue entry |
| `D` | Clear the queue |
| `d` | Delete the active playlist |

Some keys are context-dependent: `a` adds tracks in the browser but saves the
queue as a playlist elsewhere; `d` deletes the active playlist while `x` removes a
single queue entry. See [docs/features.md](docs/features.md) for the full
context-aware reference, including the playlist manager (`A` new, `r` rename).

See [docs/features.md](docs/features.md) for the complete keybinding reference.

## Configuration

Harmonium reads configuration from:

```
~/.config/harmonium/config.toml
```

A minimal configuration:

```toml
[ui]
show_album_art = true
album_art_mode = "auto"
```

See [docs/configuration.md](docs/configuration.md) for all options.

## Documentation

| Document | Content |
|----------|---------|
| [docs/features.md](docs/features.md) | Detailed functionality specification and complete keybindings |
| [docs/architecture.md](docs/architecture.md) | Module structure, data flow, design patterns, strategies |
| [docs/technical-implementation.md](docs/technical-implementation.md) | Key technical implementations and gotchas |
| [docs/configuration.md](docs/configuration.md) | Config file reference, XDG directories, runtime options |
| [docs/development.md](docs/development.md) | Build, test, commit conventions, quality gates |
| [docs/terminal-splash.md](docs/terminal-splash.md) | Startup splash: current design and rationale |

## Quality gates

Before a local commit, run the standard gates:

```bash
cargo fmt --check
cargo check --all-targets
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets --no-fail-fast -- --test-threads=1
```

## License

Harmonium declares the BSD 3-Clause License in `Cargo.toml`. The repository does
not currently provide packaged releases or a separate license file.
