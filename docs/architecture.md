# Architecture

HushKey is a single Rust binary with four layers. Only `src/audio/{windows,macos,linux}.rs` contain OS-specific audio code, and everything above the `AudioController` trait is platform-agnostic.

```
┌──────────────────────────────── main thread (Slint event loop) ─────────────────────────────┐
│  app.rs  ── state, config, UI wiring ──┬── ui/  (Slint window, toast, tray, icon)           │
│                                        ├── config/  (TOML load/save)                         │
│                                        └── hotkeys/ (register / suspend / capture parsing)   │
└───────────────▲───────────────────────────────────┬─────────────────────────────────────────┘
                │ AudioEvent (invoke_from_event_loop)│ AudioCommand (mpsc)
┌───────────────┴──────────── audio thread ─────────▼─────────────────────────────────────────┐
│  audio/worker.rs → MuteManager (toggle / mute-all state) → Box<dyn AudioController>          │
│                                                              ├─ windows.rs  WASAPI sessions  │
│                                                              ├─ macos.rs    process taps     │
│                                                              └─ linux.rs    PulseAudio       │
└──────────────────────────────────────────────────────────────────────────────────────────────┘
          ▲
          │ AudioCommand sent directly from the OS hotkey callback (no UI hop)
   global-hotkey handler
```

## Source layout

| Path | Responsibility |
|---|---|
| `src/main.rs` | CLI flags, single-instance guard, Slint backend selection, event loop |
| `src/app.rs` | Main-thread controller: owns config, hotkeys, tray, toast, window; reacts to `AudioEvent`s |
| `src/audio/mod.rs` | `AudioController` trait, `AudioApp`, `AppKey`, errors, `create_controller()` |
| `src/audio/mute.rs` | `MuteManager`: toggle and Mute All / Restore All semantics (unit-tested) |
| `src/audio/worker.rs` | Audio thread, command/event enums |
| `src/audio/windows.rs` | Windows backend (Core Audio / WASAPI) |
| `src/audio/macos.rs` | macOS backend (Core Audio process taps) |
| `src/audio/linux.rs` | Linux backend (libpulse, works with PipeWire) |
| `src/audio/null.rs` | In-memory backend for tests and unsupported OSes |
| `src/hotkeys/accelerator.rs` | `Accelerator` type, string format, Slint key-event → shortcut (unit-tested) |
| `src/hotkeys/mod.rs` | `Hotkeys`: registers config bindings with the OS, dispatches presses |
| `src/config/mod.rs` | `Config`, `ConfigStore` (atomic TOML writes, corrupt-file recovery) |
| `src/ui/` | Tray (`tray.rs`), toast popup (`toast.rs`), procedural icon (`icon.rs`) |
| `src/platform.rs` | Launch-at-login, open folder, single-instance socket |
| `ui/*.slint` | Declarative UI, compiled into Rust by `build.rs` |

## The `AudioController` trait

```rust
pub trait AudioController {
    fn backend_name(&self) -> &'static str;
    fn capabilities(&self) -> Capabilities; // per-app volume/mute/device, master volume, devices
    fn list_audio_apps(&mut self) -> AudioResult<Vec<AudioApp>>;
    fn set_app_volume(&mut self, app: &str, volume: f32) -> AudioResult<()>;
    fn set_app_mute(&mut self, app: &str, muted: bool) -> AudioResult<()>;
    fn mute_all(&mut self) -> AudioResult<MuteSnapshot>;    // default impl
    fn restore_all(&mut self, snapshot: &MuteSnapshot) -> AudioResult<()>; // default impl
    // Optional; the defaults return Unsupported / nothing:
    fn output_devices(&mut self) -> AudioResult<Vec<OutputDevice>>;
    fn set_default_output_device(&mut self, id: &str) -> AudioResult<()>;
    fn master_volume(&mut self) -> AudioResult<MasterVolume>;
    fn set_master_volume(&mut self, volume: f32) -> AudioResult<()>;
    fn set_master_mute(&mut self, muted: bool) -> AudioResult<()>;
    fn set_app_output_device(&mut self, app: &str, device: Option<&str>) -> AudioResult<()>;
    fn app_icon(&mut self, app: &AudioApp) -> Option<IconData> { None }
    fn platform_note(&self) -> Option<&'static str> { None }
}
```

Key design points:

- **Apps are identified by a stable `AppKey`, not by PID.** Bindings must survive restarts, so the key is the executable name on Windows (`spotify.exe`), the process binary on Linux (`firefox`), and the outermost `.app` bundle on macOS (`google chrome.app`). Every session or stream that shares a key is controlled together, so "mute Chrome" mutes every tab and helper process.
- **Mute uses the mixer's mute flag, not volume 0.** The OS keeps the user's level. Nothing is lost if HushKey crashes while an app is muted. On macOS the taps die with the process, so muted apps come back automatically.
- **`mute_all` returns a snapshot** of what *it* muted. `restore_all` only unmutes those apps, so apps the user had already muted stay muted. `MuteManager` keeps that snapshot and adjusts it when the user toggles an app by hand during Mute All.
- **Backends are not `Send`.** They are built *on* the audio thread by `create_controller()`. That allows thread-affine handles: COM objects in the MTA, the PulseAudio mainloop, Core Audio tap IDs.

## Threads

| Thread | What runs there | Idle cost |
|---|---|---|
| main | Slint event loop, tray, hotkey registration, toast | blocked in the OS message loop |
| `hushkey-audio` | backend + `MuteManager` | blocked on its channel (polls every 2 s only while Mute All is engaged, to catch late-starting apps) |
| `hushkey-instance` | accepts "show window" pings from second launches | blocked in `accept()` (64 KB stack) |
| OS / library threads | X11 hotkey thread (Linux), D-Bus tray (Linux), winit helpers | idle |

### Hotkey press path

1. The OS delivers the hotkey: `WM_HOTKEY` on Windows, a Carbon event on macOS, an X11 grab on Linux.
2. The `global-hotkey` callback looks up the hotkey ID and sends `AudioCommand::ToggleApp { key, from_hotkey: true }` straight to the audio thread.
3. The audio thread toggles the app (about 5–20 ms on Windows) and emits `AudioEvent::Toggled`.
4. The main thread shows the toast and flashes the tray icon.

The UI thread is not on the critical path, so a busy UI can't delay muting.

### Recording a shortcut

While a chip is in "Press keys…" mode, `Hotkeys::suspend()` unregisters every global hotkey. Otherwise the OS would swallow a combo that's already bound and the window would never see it. The key event goes through `capture_from_key_event`, which maps Slint key text to physical key codes and enforces the "letters need a modifier" rule. Hotkeys are re-registered when the capture ends.

## Memory strategy

- **Software renderer** (`renderer-software`) draws into a CPU buffer, with no GPU context or driver allocations.
- **The window is destroyed on close**, not just hidden (`App::release_window`). The rows model, icons and settings live in `App`, so reopening the window rebuilds the UI instantly.
- The toast window is created on demand and dropped after it fades out.
- App icons are extracted once per app (64×64 RGBA, about 16 KB each) and cached.

## Adding a backend

1. Create `src/audio/<os>.rs` implementing `AudioController`. Document limitations in its module comment and in `platform_note()`.
2. Add a `cfg` arm to `create_controller()` in `src/audio/mod.rs`.
3. Add platform dependencies under `[target.'cfg(...)'.dependencies]` in `Cargo.toml`.
4. Update the capability table in `README.md` and `docs/design-decisions.md`.
