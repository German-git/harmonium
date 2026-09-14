# Terminal Splash — Current Design and Rationale

Harmonium shows a short startup animation before Ratatui attaches. It is
**shipped and enabled**: `splash::ENABLED = true` in `src/splash/mod.rs`,
and `run()` in `src/main.rs` calls `splash::run(&theme, &event_sender)` right before
`terminal.attach_ratatui()`. This document describes the current
implementation and why it took this shape; earlier revisions of this file
specified a design that was removed twice before the present version
landed.

## 1. What it does

A horizontal scan line (drawn with `─`) sweeps downward through a static
ASCII logo and reveals it row by row, easing in and out. No generative
animation, no randomness, no multi-stage morph.

```text
 ─────────────          scan line above the art
  _          _
 | |_  __ _ | |_  _     rows above the line are visible
```

- **Logo**: a 4-row ASCII "harmonium", a constant in `src/splash/logo.rs`
  (`LOGO`, `LOGO_HEIGHT`, `logo_width()`).
- **Timeline** (`SplashConfig`, single source of truth in `src/splash/mod.rs`):
  `Prep` 40 ms → `Scanning` 420 ms → `Hold` 140 ms → `transition` 0 ms;
  nominal total **600 ms** (pinned by a test).
- **Phases**: `Phase::Prep | Scanning | Hold | Done`.
- **Cancellation**: `Esc` or a plain `q` ends the loop early. Other key events
  are forwarded to the shared `EventBus` with a bounded critical-send timeout;
  they are not discarded by the splash. Worker events remain queued because the
  splash does not drain the bus.
- **Colors**: theme-only. A single foreground, `theme.border_focused`, is
  used for both the logo and the scan line; no background color is painted.

## 2. Architecture

Pre-Ratatui phase using crossterm directly, inside the same alternate
screen:

```text
main()
  ├── TerminalGuard::new()             # enable_raw_mode + EnterAlternateScreen (once)
  ├── splash::run(&theme, &event_sender) # crossterm direct; no Ratatui
  │     ├── loop: poll(16 ms) → update(elapsed) → render → flush
  ├── TerminalGuard::attach_ratatui()  # ratatui Terminal over the SAME alternate screen
  └── event_loop(...)                  # the main application
```

```text
src/splash/
├── mod.rs        # run(&Theme, &EventSender) -> io::Result<Outcome>,
│                 # Splash/SplashConfig/Phase,
│                 # ENABLED switch, render loop, crossterm_color() mapping
├── animation.rs  # pure math: Scanline, smoothstep, build_rows, center_xy,
│                 # revealed_rows — unit-testable without a TTY
└── logo.rs       # static ASCII logo constants
```

### API

```rust
pub struct Outcome { pub cancelled: bool }

pub fn run(theme: &Theme, events: &EventSender) -> io::Result<Outcome>;
```

`run` returns `Ok(Outcome)` after the timeline completes or the user
cancels. I/O failures are returned for the caller to log; a failure
degrades to `tracing::warn!` and the app continues — the splash can never
block boot.

### Pacing and timing

- The loop is **clocked by real time** (`Instant`) and **throttled by
  `poll`** (16 ms, ~60 fps), never by `sleep()`. Frame-rate independence
  comes from measuring elapsed time, so animation speed survives slow
  terminals.
- Terminal size is cached at start and refreshed only on `Event::Resize`.
- All drawing goes through `queue!` into one `flush()` per frame.

### Terminal lifecycle

The splash never enters or leaves a screen. `TerminalGuard` is split in
two phases — `new()` (raw mode + alternate screen) and
`attach_ratatui()` (creates the ratatui `Terminal`) — so the splash paints
into the buffer the guard already owns, and the app continues in the same
buffer. Leaving the alternate screen is the guard's `Drop` job. On exit the
splash always restores a clean slate: `Show + ResetColor + Clear(All) +
MoveTo(0, 0)`.

### Color mapping

Ratatui and crossterm use different `Color` enums. `crossterm_color()` in
`src/splash/mod.rs` is the single crossing point (bright variants collapse
onto their base colors; indexed and RGB pass through). No color value
enters the module from anywhere else.

## 3. Why it took this shape (history)

Two earlier attempts were removed before this implementation shipped — one
integrated inside the Ratatui event loop (reverted in `1c2e0ce`/`cac71bd`,
after `fc07301`, `0fa405f`, `ed667d7`), and an earlier pre-Ratatui version
also reverted. The lessons they taught are the reasons the current code is
structured this way:

| Lesson | How the current code applies it |
|---|---|
| Never run an animation on the app's idle poll timeout (250 ms → ~4 fps) | The splash owns a dedicated 16 ms poll loop, entirely outside the event loop |
| Never drain or discard worker events while animating | The splash runs **before** the event loop starts draining, so worker events queue up; terminal keys are forwarded to the same bus and processed afterwards |
| Ratatui's diff can leave stale cells after a foreign painter | Two-phase `TerminalGuard`: the alternate screen is entered once; full `Clear` on exit; the event loop also clears on the open/close transition of the full-window settings popup |
| Multi-stage generative animation (letters → bars → wave → block-art logo) is high effort for ~2 s of decoration | Replaced by one deterministic scanline reveal; the elaborate 5-row block-art logo from the old spec was **not** shipped |
| Animation logic needs tests without a TTY | `animation.rs` is pure; `render()` writes to any `Write` and is tested against a `Vec<u8>` |

## 4. Design rules

| Aspect | Rule |
|---|---|
| Contract | `pub fn run(&Theme, &EventSender) -> io::Result<Outcome>`; the splash forwards non-cancel keys to the shared event bus but does not drain worker events |
| Colors | Only from the loaded `Theme`; mapped through `crossterm_color()` |
| Geometry | Everything is computed from the cached terminal size; rows are truncated to the right edge and clamped vertically — the splash never overflows or scrolls the screen |
| Determinism | Derived from `elapsed` + fixed constants; no RNG |
| Removability | `ENABLED = false` skips it; deleting `src/splash/`, the `mod splash;` line and the call in `run()` leaves the app untouched |
| Startup safety | Errors degrade to a warning; terminal restoration belongs to `TerminalGuard::Drop` |

## 5. Acceptance criteria (current behavior)

- [x] Nominal duration 600 ms, clocked by real time (test-pinned).
- [x] Static ASCII `harmonium` logo revealed by a downward scanline sweep.
- [x] `Esc` / `q` cancel; other keys are forwarded to the event bus.
- [x] Cursor hidden during the splash; clean, restored screen on exit.
- [x] Resize-safe: the size refreshes on `Event::Resize`, no horizontal overflow.
- [x] Theme-only colors; single `crossterm_color()` crossing point.
- [x] Pure `animation.rs` + render-to-`Vec<u8>` tests; no TTY needed in CI.
- [x] `cargo fmt --check`, `cargo clippy --all-targets --all-features -- -D warnings`,
      `cargo test` green.
