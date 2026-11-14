# CLAUDE.md

This file guides Claude Code when working in this repository. The full agent guide is [AGENTS.md](AGENTS.md); read it first. This file covers the essentials and what needs extra care.

## Project overview

HushKey: a native Rust tray app (Windows/macOS/Linux) that binds global shortcuts to mute/unmute individual apps' audio, plus a Mute All shortcut. UI is Slint with the software renderer; minimal idle memory is a hard requirement. **No webviews (Tauri/wry), no GPU renderers.**

## Key commands

```bash
cargo build                                   # debug build
cargo run                                     # run with window; `-- --minimized` for tray only
cargo run -- --list                           # print detected audio apps (real devices)
cargo test                                    # unit tests (no audio hardware needed)
cargo clippy --all-targets -- -D warnings     # lint
cargo fmt                                     # format (rustfmt.toml, width 120)
cargo check --target aarch64-apple-darwin     # type-check the macOS backend from Windows/Linux
```

## Architecture pointers

- `src/audio/mod.rs`: the `AudioController` trait. All OS audio code is behind it.
- `src/audio/{windows,macos,linux}.rs`: one backend per OS. `null.rs` is the test double.
- `src/audio/mute.rs`: toggle / Mute All / Restore All rules (snapshot-based restore).
- `src/audio/worker.rs`: the audio thread. Hotkey presses are sent here directly, bypassing the UI.
- `src/app.rs`: main-thread controller. `with_app(|a| ...)` is how callbacks reach it.
- `ui/*.slint`: the UI. `build.rs` compiles `ui/main.slint`.
- Docs: `docs/architecture.md`, `docs/design-decisions.md` (platform limitations), `docs/building.md`.

## Be careful when editing audio backend code

- **Keep platform code inside `src/audio/<os>.rs`.** Don't call OS audio APIs from `app.rs`/UI; extend the trait instead.
- **Backends live on the audio thread and are `!Send` by design** (COM apartment, PulseAudio mainloop, Core Audio taps). Don't wrap them in `Arc<Mutex<_>>` or move them.
- **Mute via the mixer mute flag, never by setting volume to 0.** This keeps the user's level and is crash-safe.
- **Group by `AppKey`, never by PID.** Bindings must survive restarts; one key controls all of an app's sessions.
- **Never fake unsupported features.** macOS can't set per-app volume: return `AudioError::Unsupported`, keep `Capabilities` honest, and document it.
- **Windows:** `ComGuard` must stay the last field of `WasapiController` (field drop order = COM release before `CoUninitialize`). Enumerate sessions on *all* active render devices, not just the default.
- **macOS:** resolve `AudioHardwareCreateProcessTap`/`DestroyProcessTap` via `dlsym`. Linking them directly breaks launch on macOS < 14.2. Taps must be destroyed in `Drop`.
- **Linux:** every PulseAudio operation must be driven to completion with `wait()` on the same thread.
- You can only truly verify a backend by running `cargo run -- --list` and muting on that OS. State plainly which OS you tested on; macOS and Linux changes made from Windows are compile-checked at best.
- Don't send synthetic keyboard/mouse input to test the UI on a developer's live desktop. Ask them to try it instead.

## Other gotchas

- Shortcut recording calls `Hotkeys::suspend()`. Every exit path must reach `App::end_capture()` so hotkeys are re-registered.
- The toast window depends on the `NEXT_WINDOW_IS_TOAST` flag consumed by the winit attributes hook in `src/ui/toast.rs`. Keep it non-activating so it never steals focus from games.
- Slint reserves property names like `row`, `col`, `x`, `y`. Pick other names.
