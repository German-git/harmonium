# Development

Build instructions, quality gates, commit conventions, and the phased roadmap.

## Prerequisites

- **Rust 1.98.0** (edition 2024, `rust-version = "1.98"`)
- A Linux terminal with at least Unicode support

The repository's `rust-toolchain.toml` pins the tested toolchain to Rust
1.98.0 and requests the `rustfmt` and `clippy` components. A newer compiler may
be useful for development, but it is not an additional supported release
profile until it has been validated with the checks below.

### System dependencies

- **PipeWire development packages** — the native audio output binds against
  the PipeWire and SPA libraries (discovered via `pkg-config`).
- **A C++ toolchain** — the `soundtouch` crate builds a C++ FFI bridge for
  pitch-preserving playback speed.
- **ALSA development libraries** — required by the default `cpal` backend
  (used for output enumeration) and rodio.
- **libclang development files** — required by bindgen when building the
  PipeWire and SPA bindings.

```bash
# Debian/Ubuntu
sudo apt install libasound2-dev libpipewire-0.3-dev pkg-config build-essential libclang-dev

# Fedora
sudo dnf install alsa-lib-devel pipewire-devel pkgconf-pkg-config gcc-c++ clang-devel

# Arch
sudo pacman -S alsa-lib pipewire base-devel pkgconf clang
```

## Supported build profile

The only supported profile is the current Linux/PipeWire build. The application
uses a native PipeWire output sink and the repository has not established a
supported Windows, macOS, PulseAudio-only, ALSA-only, JACK, or other alternate
backend profile. The package has no Cargo feature definitions, so native
integrations are intentionally compiled for every build:

| Integration | Why it is always compiled |
|-------------|---------------------------|
| PipeWire and SPA | Native audio output and sink control |
| `cpal` and ALSA | Output enumeration and native audio support dependencies |
| SoundTouch | Pitch-preserving playback speed through the bundled C++ FFI bridge |
| Image and terminal adapters | Artwork protocol detection and Unicode fallback |
| Network and external-process adapters | Stream, lyrics, artwork, and optional provider paths |

This is a support boundary, not an invitation to add speculative features.
`--all-features` is harmless validation for this featureless package, while
`--no-default-features`, `--features pipewire`, and similar guessed combinations
are unsupported and should not be published as alternate builds.

### Current support matrix

| Combination | Status | Notes |
|-------------|--------|-------|
| Linux terminal + Rust 1.98.0 + PipeWire output | **Supported** | The tested build profile |
| Linux + a working PipeWire session manager | **Required at runtime** | WirePlumber is the expected and tested session-manager path; another manager is not an explicit support commitment |
| Unicode-capable terminal | **Required** | The UI baseline works without a graphics protocol |
| Kitty, Sixel, or iTerm2 artwork protocol | **Supported when detected** | The external protocol is capability-dependent and falls back when unavailable |
| `yt-dlp`, `ueberzugpp`, `ueberzug`, or `xwininfo` installed | **Optional** | Missing tools disable only the capability that needs them |
| Windows, macOS, or another non-Linux host | **Unsupported** | No validated native dependency or runtime profile |
| PulseAudio-only, ALSA-only, JACK, or another alternate audio backend | **Unsupported** | No proven backend selection or release validation |
| Cargo feature combinations or feature-disabled native integrations | **Unsupported** | No Cargo feature matrix exists |

The distro commands above are developer prerequisite examples. They are not
package recipes, binary distribution commitments, or proof of support for every
distribution that can provide similarly named packages.

## Build

```bash
cargo build              # debug build
cargo build --release    # optimized build
```

## Run

```bash
cargo run                # debug mode
cargo run --release      # optimized mode
```

The application starts in `$XDG_MUSIC_DIR` when set, otherwise
`$HOME/Music/`, falling back to `$HOME/`.

## Quality gates

Before a local commit, run the standard gates:

```bash
cargo fmt --check
cargo check --all-targets
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets --no-fail-fast -- --test-threads=1
git diff --check
```

These are local quality gates, not a checked-in CI workflow. A warning promoted
by `-D warnings`, a compilation failure, a test failure, a formatting failure,
or whitespace error blocks the commit. The release gate below adds locked
dependency and documentation checks.

## Supply-chain checks

Install the pinned tools and run both dependency checks locally:

```bash
cargo install --locked --version 0.22.2 cargo-audit
cargo install --locked --version 0.20.2 cargo-deny
cargo audit
cargo deny check
```

The accepted dependency licenses are MIT, Apache-2.0, Apache-2.0 WITH
LLVM-exception, BSD-2-Clause, BSD-3-Clause, ISC, Zlib, 0BSD, Unlicense,
LGPL-2.1, MPL-2.0, CDLA-Permissive-2.0, Unicode-3.0, and BSL-1.0, including
the SPDX alternatives and exceptions present in the locked graph.
LGPL-2.1 is intentionally accepted for SoundTouch and must not be broadened
silently.

## Release, packaging, and tool matrix

Until a distribution format is selected, the practical supported artifact is a
locally built Linux binary produced from this repository with the documented
native dependencies. The matrix below separates release requirements from
optional conveniences without promising a package ecosystem that does not yet
exist.

| Area | Required for a supported artifact | Optional or capability-dependent | Not a current commitment |
|------|-----------------------------------|----------------------------------|---------------------------|
| Rust/toolchain | Rust 1.98.0 with `rustfmt` and `clippy`; use the pinned `rust-toolchain.toml` | A newer toolchain may be used for investigation only | Compatibility with an unvalidated compiler range |
| Native build packages | PipeWire/SPA development files, `pkg-config`, ALSA development files, a C++ compiler, and `libclang` | Distribution-specific package names | A universal package recipe or a fixed distro version matrix |
| PipeWire runtime | A running PipeWire session with an available output sink | WirePlumber diagnostics such as `wpctl status` | A supported non-PipeWire audio daemon |
| Artwork tools | Unicode-capable terminal | `ueberzugpp`; legacy `ueberzug` plus `xwininfo` for its geometry path | Bundling or requiring an external-artwork tool |
| Stream tool | Core local playback does not require `yt-dlp` | `yt-dlp` for the YouTube provider | Bundling, downloading, or version-pinning `yt-dlp` |
| Terminal | UTF-8/Unicode display and a functioning alternate-screen terminal | Kitty, Sixel, iTerm2, or another detected image protocol | A guarantee for terminals that cannot display the baseline UI |
| Validation | The locked Cargo checks and artifact smoke tests below | `cargo audit` and `cargo deny` installed locally or in release infrastructure | A release workflow, signing service, or update channel |

### Release validation commands

Run these from a clean checkout with the pinned toolchain. `cargo audit` and
`cargo deny` must be installed before their checks are meaningful.

```bash
rustup show active-toolchain
cargo fmt --all -- --check
cargo check --locked --all-targets
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo test --locked --all-targets --no-fail-fast
cargo test --locked --doc --no-fail-fast
cargo audit
cargo deny check
```

The `--all-features` flags above validate the featureless manifest; they do not
enable an alternate backend. Record the exact compiler, native package, and
dependency-check versions alongside the release candidate.

### Artifact smoke tests

After building `target/release/harmonium`, perform the following bounded checks
on the same Linux/PipeWire profile:

- Start the binary in a Unicode-capable terminal and confirm the terminal is
  restored after a normal quit.
- Browse to a known local audio file, play it, pause it, seek, and quit. Confirm
  that the expected PipeWire sink is receiving audio.
- Run `./target/release/harmonium --pipewire-test` and inspect the one-second
  tone and `wpctl status` output when WirePlumber is available.
- Open Settings → Sound, confirm sink discovery, and verify that an unavailable
  saved sink falls back to the session default without destroying the saved ID.
- With each optional tool installed, exercise its capability once. Without the
  tool, confirm that startup and local playback still work and that artwork or
  YouTube resolution degrades with a warning rather than blocking the app.
- Verify that a release build contains the expected binary and that its dynamic
  native dependencies resolve on the target machine before distribution.

These are smoke checks, not claims of broad terminal, decoder, provider, or
external-process integration coverage.

### Configuration and state migration checklist

Before replacing a working installation:

- [ ] Quit Harmonium and back up the config, data, and cache directories.
- [ ] Preserve `config.toml` and `themes/` under `XDG_CONFIG_HOME` (normally
      `~/.config/harmonium/`).
- [ ] Preserve playlists and `state.toml` under `XDG_DATA_HOME` (normally
      `~/.local/share/harmonium/`).
- [ ] Preserve the cache only when useful for diagnostics or recovery. Logs and
      other cache data are reconstructible.
- [ ] If upgrading from a version that stored preferences in `state.toml`, let
      the application migrate those values into `config.toml` under `[general]`,
      then review the written configuration.
- [ ] Confirm that `output_sink_id` still names an available PipeWire sink. An
      unavailable sink should fall back for the session without deleting the
      preference.
- [ ] Start the new binary once, verify settings, playlists, resume behavior,
      and themes, then keep the backup until the smoke tests pass.

On rollback, stop the application, restore the previous binary, and restore the
backed-up configuration or state only if the previous binary cannot read the
newly written files. Do not delete playlists or user themes as part of a binary
rollback.

### Explicit packaging limits

This repository currently does **not** promise `.deb`, `.rpm`, Arch packages,
AppImage, Flatpak, Snap, Homebrew, Nix, container images, signed binaries,
automatic updates, or Windows/macOS artifacts. It also does not promise a
distro-independent native dependency installer. Selecting a distro and format,
defining package ownership and upgrade semantics, and adding a release workflow
are product decisions for a later change.

### GitHub release automation

The repository's release automation is split into two decisions:

1. `release-plz.yml` creates the version/changelog pull request and, after it is
   merged, creates the semantic `vMAJOR.MINOR.PATCH` GitHub release.
2. `release.yml` runs only for the published release, builds the optimized Linux
   binary, creates the `.tar.gz` archive, and uploads it to that existing release.

The `release` job in `release-plz.yml` requires a repository secret named
`RELEASE_TOKEN` containing a PAT or GitHub App token with permission to create
contents/releases. The default `GITHUB_TOKEN` is intentionally not used there:
GitHub suppresses workflow events generated by that token, which would prevent
`release.yml` from packaging the published release.

### Gate details

| Gate | What it checks | Fail means |
|------|---------------|------------|
| `cargo fmt --check` | Code follows `rustfmt` defaults | Reformat with `cargo fmt` |
| `cargo check` | Type checking, borrow checking | Compilation error |
| `cargo clippy -D warnings` | Idiomatic Rust, common mistakes | Warning treated as error |
| `cargo test` | All unit and integration tests | Regression or missing test |

## Commit conventions

### Format

Commits follow [Conventional Commits](https://www.conventionalcommits.org/)
with the repository's established concise English subject convention:

```
<type>(<scope>): <concise English description>
```

### Types

| Type | Use |
|------|-----|
| `feat` | New feature or capability |
| `fix` | Bug fix |
| `style` | Formatting, no code change |
| `refactor` | Code restructuring, no behavior change |
| `test` | Adding or updating tests |
| `docs` | Documentation only |
| `chore` | Build, deps, tooling |

### Rules

- No `"Co-Authored-By"` or AI attribution
- Description in concise, professional English, consistent with existing commits
- One logical change per commit
- Never push (local git only, per owner directive)

## Comment style

Source code comments follow these rules:

- **Language**: English
- **Purpose**: explain **WHY**, not WHAT
- **No semicolons** in comments
- **No new `unwrap()`/`expect()`** in recoverable production paths. Narrow invariant
  exceptions are allowed only when the invariant is established locally and a
  nearby comment names it: for example, `TerminalGuard::terminal_mut` requires
  `attach_ratatui()` first, and the static help table must contain every
  `ReservedHelpId`. Exhaustive `unreachable!` branches follow the same rule.
  Propagate user, filesystem, network, decoder, and terminal errors instead of
  turning them into process-wide panics.
- Obvious code needs no comment

```rust
// GOOD: explains why the staleness gate exists
// The worker keeps its capture time label until the next Play,
// so remapping track_index would defeat the staleness check.
if self.playback.track_index != Some(i) {
    return;
}

// BAD: explains what the code does (obvious from reading it)
// Check if track_index equals i
if self.playback.track_index != Some(i) {
    return;
}
```

## Phased roadmap

The original phase scopes are retained as historical project milestones:

| Phase | Scope | Status |
|-------|-------|--------|
| 1 | Project, configuration, terminal, Ratatui, event loop, layout | Done |
| 2 | File browser: navigation, multi-select, add tracks | Done |
| 3 | Metadata extraction, playlist queue, M3U persistence | Done |
| 4 | Audio: play/pause, next/previous, seek, volume | Done |
| 5 | Repeat/shuffle combinations, selection engine | Done |
| 6 | Album artwork, capability detection, Unicode fallback | Done |
| 7 | Themes, keybindings, advanced configuration | Done |
| 8 | Optimization, tests, logging, and errors | Done; packaging deferred |

Substantial feature work also landed after Phase 7: the playlist manager,
lyrics/karaoke panel, full settings editor, native PipeWire output, playback
speed, crossfade, preamp gain, the sort engine, the config/state split, stream
providers, stream cancellation, and the startup splash.

Packaging, signing, update channels, and the first release remain outside the
current repository support commitment.

Each phase leaves the project **compilable and functional**. No phase
introduces a breaking change that prevents the application from running.

## Testing

### Running all tests

```bash
cargo test
```

### Running specific tests

```bash
cargo test artwork          # tests matching "artwork"
cargo test input            # tests matching "input"
cargo test -- --nocapture   # show println! output
```

### Test categories

| Category | Description |
|----------|-------------|
| Unit tests | Pure domain logic (state transitions, key mapping, layout math) |
| Fixture tests | tempfile-based I/O tests (loader, config, playlist) |
| Render tests | `TestBackend` tests that pin widget rendering without a real terminal |
| Integration tests | EventBus round-trip, staleness gates, command dispatch |

### Writing tests

- Use the shared RAII `TestTempDir` fixture (or `unique_temp_dir`) for filesystem tests; it removes the directory on drop
- Use `TestBackend` for render tests (no real terminal needed)
- Test boundary conditions: empty inputs, zero caps, overflow
- One logical assertion per test, test name describes the scenario
- No artificial tests that merely increase coverage numbers
