# Building HushKey

All platforms use the stable Rust toolchain (MSRV 1.82, check `rust-version` in `Cargo.toml`). Install it with [rustup](https://rustup.rs).

```bash
cargo build              # debug build, console logs visible
cargo build --release    # optimized: fat LTO, stripped, panic=abort
cargo test               # unit tests (config, accelerators, mute logic, icon)
```

Logging uses `env_logger`. Set `RUST_LOG=hushkey=debug` for more detail. Release builds on Windows have no console, so redirect output to a file if you need logs: `hushkey.exe > log.txt 2>&1`.

Diagnostics: `hushkey --list` prints every detected audio app with its key, volume and mute state.

---

## Windows

**Requirements:** Windows 10 1809+ or Windows 11, plus the MSVC toolchain (Visual Studio Build Tools with the "Desktop development with C++" workload).

```powershell
cargo build --release
.\target\release\hushkey.exe
```

- No runtime dependencies and no installer needed. The exe is self-contained.
- Config: `%APPDATA%\HushKey\config\config.toml`
- Launch at login writes `HKCU\Software\Microsoft\Windows\CurrentVersion\Run\HushKey`.
- **Gotcha:** if a game runs elevated (as administrator) and HushKey does not, hotkeys still fire, but some anti-cheat systems block other processes' audio sessions from being changed. Running HushKey elevated is a workaround, not a requirement.

## macOS

**Requirements:** macOS 14.2+ for per-app muting (older versions list apps read-only), plus the Xcode Command Line Tools (`xcode-select --install`).

```bash
cargo build --release
```

### Permissions and bundling

Per-app muting uses Core Audio process taps, which are gated by the **Screen & System Audio Recording** permission. macOS only shows the permission prompt for an **app bundle** whose `Info.plist` contains a usage string:

```xml
<key>NSAudioCaptureUsageDescription</key>
<string>HushKey mutes individual apps by attaching a muted audio tap to them. No audio is recorded.</string>
<key>LSUIElement</key>
<true/>
```

`LSUIElement` hides the Dock icon (HushKey also sets the accessory activation policy at runtime). One way to bundle is [`cargo-bundle`](https://github.com/burtonageo/cargo-bundle). Add the keys above to the generated plist.

If muting fails with a permission message, open **System Settings → Privacy & Security → Screen & System Audio Recording** and enable HushKey. Global hotkeys use the Carbon API and need **no** Accessibility permission.

- Config: `~/Library/Application Support/HushKey/config.toml`
- Launch at login installs a LaunchAgent in `~/Library/LaunchAgents/`.

## Linux

**Requirements:** a PulseAudio server, or PipeWire with `pipewire-pulse` (the default on Fedora 34+, Ubuntu 22.10+, Debian 12+ and Arch). An X11 session is needed for global hotkeys; see Wayland below.

Build dependencies:

| Distro | Command |
|---|---|
| Debian / Ubuntu | `sudo apt install build-essential pkg-config libpulse-dev libfontconfig1-dev libxkbcommon-dev` |
| Fedora | `sudo dnf install gcc pkgconf-pkg-config pulseaudio-libs-devel fontconfig-devel libxkbcommon-devel` |
| Arch | `sudo pacman -S base-devel pkgconf libpulse fontconfig libxkbcommon` |

```bash
cargo build --release
```

- Tray: StatusNotifierItem over D-Bus. KDE, XFCE, Cinnamon and others support it out of the box. On GNOME, install the *AppIndicator and KStatusNotifierItem Support* extension. Without a tray, HushKey keeps its window open so it stays reachable.
- Config: `$XDG_CONFIG_HOME/hushkey/config.toml` (usually `~/.config/hushkey/`)
- Launch at login writes `~/.config/autostart/HushKey.desktop`.

### Wayland

Wayland has no protocol that lets an app grab global shortcuts. Under a Wayland session HushKey's hotkeys only fire while an XWayland window has focus. Workarounds:

1. Log into an X11 session, **or**
2. Bind the shortcut in your desktop's own keyboard settings to a command. This is a known gap: HushKey has no CLI toggle command yet. Contributions welcome (see [CONTRIBUTING.md](../CONTRIBUTING.md)).

---

## Cross-checking other platforms

From any OS you can type-check the macOS backend without a Mac:

```bash
rustup target add aarch64-apple-darwin
cargo check --target aarch64-apple-darwin
```

Cross-checking Linux from Windows/macOS fails inside native font dependencies (`fontconfig`/`pkg-config`). Use CI, a container, or WSL with the build dependencies above.
