# Contributing to HushKey

Thanks for helping! HushKey spans three operating systems with very different audio stacks, so testing on real machines is the most valuable contribution of all. A bug report from a Mac or a Wayland desktop is genuinely useful.

## Development setup

1. Install stable Rust via [rustup](https://rustup.rs).
2. Install your platform's system dependencies, listed in [docs/building.md](docs/building.md).
3. Clone and run:

   ```bash
   git clone https://github.com/X4Lo/HushKey.git
   cd HushKey
   cargo run
   ```

4. Useful while developing:

   ```bash
   cargo test                                  # unit tests, no audio hardware needed
   cargo run -- --list                         # what does the backend see on this machine?
   RUST_LOG=hushkey=debug cargo run            # verbose logs
   ```

Read [docs/architecture.md](docs/architecture.md) before changing anything under `src/audio/` or `src/app.rs`.

## Coding style

- `cargo fmt` (config in `rustfmt.toml`) and `cargo clippy --all-targets -- -D warnings` must pass. CI enforces both.
- Keep OS-specific code in `src/audio/<os>.rs` behind the `AudioController` trait. Small `#[cfg]` blocks elsewhere are fine when unavoidable (window attributes, tray quirks).
- Don't fake capabilities a platform lacks. Return `AudioError::Unsupported` and document the limitation in the module comment, `platform_note()` and `docs/design-decisions.md`.
- No `unwrap()`/`expect()` on OS or I/O calls in app code. Surface errors to the user via the status banner or toast.
- Pure logic (parsing, state machines) gets unit tests next to the code (`#[cfg(test)] mod tests`).
- UI: edit `ui/*.slint`. Use `Theme` tokens from `ui/theme.slint` instead of hard-coded colours, and set accessibility properties (`accessible-role`, `accessible-label`) on interactive elements.
- Keep the idle footprint small: no polling while the window is hidden, no new background threads without a good reason, no webviews or GPU renderers.

## Branches and pull requests

- Branch from `main`: `feat/<short-name>`, `fix/<short-name>`, `docs/<short-name>`.
- Commit messages follow [Conventional Commits](https://www.conventionalcommits.org): `feat(audio): …`, `fix(macos): …`, `docs: …`.
- Keep PRs focused: one feature or fix per PR.
- In the PR description, **list the OSes you actually ran the change on** (e.g. "Tested: Windows 11 24H2. Compile-checked: macOS. Not tested: Linux"). Reviewers rely on this, because CI can only build and run unit tests.
- Update the docs when behaviour or platform support changes (README capability table, `docs/`).
- CI must be green on all three OSes before merge.

## Reporting bugs

Please use the issue templates. Platform-specific details matter more than usual here:

- **OS and version**, e.g. Windows 11 23H2, macOS 15.1 (Apple Silicon), Ubuntu 24.04.
- **Linux:** session type (`echo $XDG_SESSION_TYPE`), desktop environment, and audio server (`pactl info | grep "Server Name"`).
- **macOS:** whether HushKey was run from an app bundle, and whether *Screen & System Audio Recording* permission is granted.
- **HushKey version**, shown in Settings → About.
- **Output of `hushkey --list`** while the problem app is playing.
- **Logs:** run with `RUST_LOG=hushkey=debug` and attach the output. On Windows release builds, redirect to a file: `hushkey.exe > log.txt 2>&1`.
- What you pressed, what you expected, and what happened. For shortcut problems, say which keyboard layout you use.

## Security

Please report security issues privately via GitHub's "Report a vulnerability" feature rather than a public issue.
