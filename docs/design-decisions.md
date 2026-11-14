# Design decisions

## UI toolkit: Slint, not Tauri / egui / iced

HushKey is a utility that runs all day in the tray. Idle footprint and responsiveness matter more than anything else.

| Option | Why not / why |
|---|---|
| **Tauri** | Ships a system WebView (WebView2 / WebKitGTK). Even an idle window costs tens of MB of RAM across several helper processes. Rejected: too heavy for an always-on tray utility. |
| **egui** | Immediate mode: redraws the whole UI each frame while interacting, and usually runs on a GPU context (wgpu/glow) with its driver allocations. Its default look is a "developer tool" aesthetic. |
| **iced** | Good architecture, but pulls in wgpu by default and is heavier to theme for this kind of polished card UI. |
| **Slint** ✅ | Declarative `.slint` markup compiled to native Rust at build time. Retained mode: only dirty regions repaint. Has a **software renderer**, so no GPU context or driver memory. Animations, rounded corners, shadows and gradients are built in, giving a modern look cheaply. Exposes winit hooks, which we need for the non-activating toast window. |

Licensing: Slint is GPLv3 / Royalty-free / commercial. HushKey (MIT) uses it under the Royalty-free license, which requires attribution, shown in the app's About section. See [LICENSE.md](../LICENSE.md).

Further memory choices:

- The main window is **destroyed** on close and rebuilt on demand. All state lives in `App`.
- The toast is created per notification and dropped after it fades.
- Nothing polls while hidden. The audio thread blocks on its channel, except for a 2-second check while Mute All is engaged.

## License: MIT

MIT is the norm in the Rust ecosystem: most of our dependencies are MIT/Apache-2.0. It's short and lets anyone package or fork the app. Apache-2.0's patent grant matters little for a small desktop utility with no novel algorithms, so the simpler license wins.

## Mute flag instead of "volume → 0"

The original spec described muting as "set volume to 0, restore the previous volume". HushKey uses the OS mixer's **mute flag** instead, on every platform that has one:

- The user's volume level is preserved by the OS itself, so there's no saved state to lose.
- If HushKey crashes or is killed while apps are muted, nothing is stuck at 0%. The user unmutes in the system mixer as usual. On macOS, taps are released automatically when the process exits.
- The OS mixer UI shows the app as muted, which matches what the user sees in HushKey.

`set_app_volume` is still implemented on Windows and Linux as part of the backend contract.

## Stable app identity

Bindings are keyed by executable name / bundle, never by PID, so they survive app and HushKey restarts. All sessions or streams of one app are controlled together. Bound apps that aren't running stay listed (greyed out) so their shortcut can still be edited.

## Shortcut rules

- Shortcuts use **physical key codes** (what the OS hotkey APIs register). On non-US layouts, the stored key is the US-layout position.
- Letters, digits, punctuation, Space, Enter and arrows **require Ctrl, Alt or Win/Cmd**, so a binding can't hijack normal typing. Shift alone doesn't count. F1–F24, Pause, Scroll Lock and Insert may be used alone.
- While recording a shortcut, all global hotkeys are temporarily unregistered so the combo reaches the window.
- Conflicts with another HushKey binding are rejected up front. Conflicts with other programs are reported when registration fails.

## Feedback without stealing focus

The toast window is created with winit `active = false`, no decorations, always-on-top, skip-taskbar (Windows) and cursor hit-testing disabled. Taking focus would minimize exclusive-fullscreen games and interrupt typing. Games in true exclusive fullscreen draw over every window, so the toast can't show there. The tray icon still flashes and the result is still applied. Most modern games use borderless fullscreen, where the toast is visible.

---

## Platform limitations

### macOS per-app audio

macOS has **no public API to change another process's volume**. The CoreAudio HAL exposes device volume only. The established approaches:

1. **Virtual audio driver** (SoundSource, Background Music, eqMac): route all audio through a HAL plug-in that mixes per-app. This needs an installer, admin rights and a system extension. It's invasive, fragile across macOS updates, and far beyond a hotkey utility.
2. **Core Audio process taps** (`AudioHardwareCreateProcessTap`, macOS 14.2+): a tap created with `muteBehavior = CATapMuted` silences the tapped process while the tap exists.

**HushKey uses option 2.** As a result:

- ✅ Per-app **mute/unmute** works (that's all HushKey's shortcuts need).
- ❌ Per-app **volume levels** are not possible. The backend reports `per_app_volume: false`, the UI hides volume bars, and `set_app_volume` returns `Unsupported` for values between 0 and 1. It never fakes a level.
- Requires **macOS 14.2+**. The tap functions are resolved with `dlsym` at runtime, so the binary still launches on older macOS. There, apps are listed read-only and a banner explains why.
- Requires the **Screen & System Audio Recording** permission. The app bundle must declare `NSAudioCaptureUsageDescription` (see [building.md](building.md#macos)).
- Apps are grouped by their outermost `.app` bundle, so helper processes (e.g. Chrome's renderer helpers) are muted together. When an app spawns a new helper while muted, the tap is rebuilt on the next refresh.

> ⚠️ The macOS backend was written and compile-checked (`cargo check --target aarch64-apple-darwin`) but **not yet run on real hardware**. Please report results.

### Wayland global shortcuts

Wayland deliberately has no API for one client to grab keys globally. The `global-hotkey` crate uses X11 `XGrabKey`, which under Wayland only sees XWayland clients. The XDG `GlobalShortcuts` portal exists but isn't supported by the hotkey crate and isn't implemented by every compositor. Registration appears to succeed through XWayland but never fires for native Wayland windows, so HushKey checks `XDG_SESSION_TYPE` at startup and shows a warning banner. A future version could adopt the portal and add a CLI (`hushkey toggle <app>`) for desktop-level bindings.

### Linux tray on GNOME

GNOME Shell doesn't show StatusNotifierItem icons without an extension. If no tray host is found, HushKey keeps its window open so the app stays reachable.

### Windows

The Windows backend is fully native. Sessions are enumerated on every active output device, not just the default one, so apps routed to a headset or virtual device are found. Protected processes HushKey can't open are still controllable; they're listed as "Process <pid>" with a PID-based key, which can't be bound persistently.
