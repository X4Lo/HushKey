# AGENTS.md

Guidance for AI coding agents working in this repository. Human contributors: see [CONTRIBUTING.md](CONTRIBUTING.md).

## What this is

HushKey is a Rust tray app that binds **global keyboard shortcuts** to **mute/unmute individual applications' audio**, plus a Mute All shortcut. One codebase for Windows, macOS and Linux. The UI is [Slint](https://slint.dev) with the **software renderer**. **Do not introduce a webview (Tauri, wry, Electron) or a GPU renderer**: low idle memory is a core product requirement.

## Project structure

```
src/
  main.rs            CLI flags, single-instance guard, Slint backend selection, event loop
  app.rs             main-thread controller (config, hotkeys, tray, toast, window lifecycle)
  platform.rs        launch-at-login, open-folder, single-instance socket
  audio/
    mod.rs           AudioController trait, AudioApp/AppKey/AudioError, create_controller()
    mute.rs          MuteManager: toggle / mute-all / restore-all semantics  ← unit tests
    worker.rs        audio thread; AudioCommand in, AudioEvent out
    windows.rs       Windows backend: WASAPI IAudioSessionManager2 / ISimpleAudioVolume
    macos.rs         macOS backend: Core Audio process taps (14.2+), mute-only
    linux.rs         Linux backend: libpulse sink inputs (works on PipeWire)
    null.rs          in-memory backend for tests / unsupported OSes
  hotkeys/
    accelerator.rs   Accelerator type, "Ctrl+Alt+S" format, Slint key → shortcut  ← unit tests
    mod.rs           Hotkeys: register config bindings, suspend during capture
  config/mod.rs      Config + ConfigStore (TOML, atomic writes)  ← unit tests
  ui/
    mod.rs           helpers (avatar tint, initials, icon conversion)
    icon.rs          procedural app/tray icon rasteriser  ← unit tests
    tray.rs          tray icon + menu
    toast.rs         non-activating on-screen confirmation window
ui/                  .slint files (main.slint, widgets.slint, theme.slint, toast.slint)
build.rs             compiles ui/main.slint (fluent style)
docs/                architecture.md, building.md, design-decisions.md
```

## Build / run / test

| Task | Command |
|---|---|
| Build (debug) | `cargo build` |
| Run | `cargo run` (window opens) · `cargo run -- --minimized` (tray only) |
| List detected audio apps | `cargo run -- --list` |
| Unit tests | `cargo test` |
| Lint | `cargo clippy --all-targets -- -D warnings` |
| Format | `cargo fmt` |
| Release | `cargo build --release` (fat LTO; slow, a few minutes) |
| Type-check macOS from anywhere | `rustup target add aarch64-apple-darwin && cargo check --target aarch64-apple-darwin` |

Per-OS system dependencies are in [docs/building.md](docs/building.md). On Linux you need `libpulse-dev`, `libfontconfig1-dev`, `libxkbcommon-dev` and `pkg-config`.

Tests don't touch real audio devices: they use `audio::null::NullController`. Backend code can only be verified by running the app on that OS. Say clearly which OS you actually ran on.

## Conventions

- **Platform code stays in `src/audio/<os>.rs`** (and small `cfg` blocks in `platform.rs`, `ui/toast.rs`, `main.rs`). Everything else must compile and behave identically on all OSes.
- **New capabilities go through the `AudioController` trait.** Never call an OS audio API from `app.rs` or the UI.
- **Backends are created on the audio thread** and are not `Send`. Don't move them across threads or put them in `Arc<Mutex<..>>`.
- **Never fake a capability.** If an OS can't do something (macOS volume levels), return `AudioError::Unsupported`, set `Capabilities` accordingly, and explain it in the module doc comment, `platform_note()`, and `docs/design-decisions.md`.
- **Mute with the mixer's mute flag**, not by setting volume to 0 (crash-safety; see design-decisions).
- Errors: `AudioResult<T>` in audio code; user-facing strings go through `App::set_status` or the toast. No `unwrap()` on OS calls.
- UI strings live in `.slint` files; keep them short, sentence case.
- Slint gotcha: `row`, `col`, `x`, `y`, `width`, `height` are built-in properties. Don't name custom properties after them.
- Formatting: `rustfmt.toml` (max width 120). Run `cargo fmt` before finishing.
- Commit messages: Conventional Commits (`feat(audio): …`, `fix(ui): …`).

## Gotchas

- **Hotkeys must be registered on the main thread** (Windows: the message-loop thread; macOS: requires the main run loop). `Hotkeys` is created in `App::start`.
- **While recording a shortcut, all hotkeys are unregistered** (`Hotkeys::suspend`). Forgetting to call `end_capture` leaves the user with no working shortcuts.
- **The tray must be created after the event loop starts** (macOS requirement). It's done in a zero-delay `slint::Timer`.
- **The toast must never take focus.** It relies on `ui::toast::window_attributes_hook` and the thread-local `NEXT_WINDOW_IS_TOAST` flag set immediately before `Toast::new()`. Any new window type created between those calls would inherit the toast attributes.
- **The main window is dropped on close** (`App::release_window`). Never hold a strong `MainWindow` handle in a closure. Use `with_app(|a| ...)` from callbacks.
- **Windows COM:** `WasapiController` initialises COM (MTA) on the audio thread and uninitialises in `ComGuard`, which must remain the **last** struct field.
- **macOS:** tap functions are resolved with `dlsym`. Don't link them directly or the app won't launch on macOS < 14.2. Muting requires the Screen & System Audio Recording permission plus `NSAudioCaptureUsageDescription` in the bundle's Info.plist.
- **Linux:** global hotkeys are X11-only. Under Wayland they appear to register but don't fire for native Wayland windows.
- **Permissions summary:** Windows needs none. macOS needs audio-capture permission for muting (no Accessibility permission for hotkeys). Linux needs an X11 session for hotkeys and a StatusNotifierItem host for the tray.
- The single-instance guard binds `127.0.0.1:47219`. A second launch asks the first to show its window and exits.
