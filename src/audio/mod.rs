//! Platform-neutral audio control.
//!
//! Every OS backend implements [`AudioController`]; nothing outside `src/audio/`
//! knows which one is active. Backends are created *on the audio worker thread*
//! (see [`worker`]) so they may hold thread-affine handles (COM objects, the
//! PulseAudio mainloop, CoreAudio taps) without being `Send`.
//!
//! | OS      | Mechanism                               | Per-app volume | Per-app mute | Per-app device |
//! |---------|-----------------------------------------|----------------|--------------|----------------|
//! | Windows | WASAPI `IAudioSessionManager2` sessions | yes            | yes          | yes            |
//! | Linux   | PulseAudio / PipeWire-pulse sink inputs | yes            | yes          | yes            |
//! | macOS   | Core Audio process taps (macOS 14.2+)   | **no**         | yes          | **no**         |
//!
//! System volume, system mute and switching the default output device work on all three.
//!
//! macOS has no public API for setting another process's volume or output device. HushKey
//! mutes an app by creating a *muted process tap* on it, which silences that process while
//! the tap exists. Arbitrary volume levels are not possible — the backend reports
//! `per_app_volume: false` / `per_app_device: false` and the UI hides those controls
//! instead of faking them.

mod mute;
pub mod worker;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(any(test, not(any(target_os = "windows", target_os = "macos", target_os = "linux"))))]
mod null;
#[cfg(target_os = "windows")]
mod windows;

pub use mute::MuteManager;

use std::fmt;
use std::path::PathBuf;

/// Stable identifier for an application across restarts — *not* a PID.
///
/// - Windows: lower-cased executable file name (`spotify.exe`), or `system-sounds`.
/// - Linux:   `application.process.binary` (`firefox`), falling back to `application.name`.
/// - macOS:   bundle identifier (`com.spotify.client`), falling back to executable name.
///
/// All audio sessions/streams sharing a key are controlled together (e.g. every
/// Chrome tab), which is what users expect from "mute Chrome".
pub type AppKey = String;

#[derive(Debug, Clone, PartialEq)]
pub struct AudioApp {
    pub key: AppKey,
    /// Friendly name (`Spotify`, `Google Chrome`).
    pub name: String,
    pub pids: Vec<u32>,
    /// 0.0..=1.0, averaged across sessions. `None` if the backend can't read it.
    pub volume: Option<f32>,
    /// `true` only if *every* session of this app is muted.
    pub muted: bool,
    /// Executable path for icon lookup, when known.
    pub exe_path: Option<PathBuf>,
    /// [`OutputDevice::id`] this app is explicitly routed to; `None` = follows the system default.
    pub device: Option<String>,
    /// Whether this app's output device can be changed (false for e.g. Windows system sounds).
    pub routable: bool,
}

/// An active playback device (speakers, headphones, HDMI, virtual cable…).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputDevice {
    /// Backend-specific stable id (MMDevice id, PulseAudio sink name, Core Audio UID).
    pub id: String,
    pub name: String,
    pub is_default: bool,
}

/// Volume and mute of the system default output device.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MasterVolume {
    /// 0.0..=1.0. `None` when the device has no volume control (some HDMI/USB outputs).
    pub volume: Option<f32>,
    pub muted: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Capabilities {
    pub per_app_volume: bool,
    pub per_app_mute: bool,
    /// Per-app output device routing.
    pub per_app_device: bool,
    /// System volume / mute of the default output device.
    pub master_volume: bool,
    /// Listing output devices and switching the system default.
    pub output_devices: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AudioError {
    /// No running app has this key right now.
    NotRunning(AppKey),
    /// The platform can't do this (see the capability table above).
    Unsupported(&'static str),
    /// The OS needs a permission the user hasn't granted (macOS audio capture).
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    PermissionDenied(String),
    Backend(String),
}

impl fmt::Display for AudioError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AudioError::NotRunning(k) => write!(f, "{k} isn't running"),
            AudioError::Unsupported(what) => write!(f, "not supported on this platform: {what}"),
            AudioError::PermissionDenied(m) => write!(f, "permission needed: {m}"),
            AudioError::Backend(m) => write!(f, "{m}"),
        }
    }
}

impl std::error::Error for AudioError {}

pub type AudioResult<T> = Result<T, AudioError>;

/// RGBA8 icon pixels (premultiplied: no), row-major.
#[derive(Debug, Clone)]
pub struct IconData {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

/// What was muted by a `mute_all` call, so `restore_all` only undoes HushKey's own work
/// and leaves apps the user had muted beforehand alone.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MuteSnapshot {
    pub muted_keys: Vec<AppKey>,
}

pub trait AudioController {
    fn backend_name(&self) -> &'static str;

    fn capabilities(&self) -> Capabilities;

    /// Every app with an audio session/stream registered with the OS mixer — including
    /// idle ones that aren't currently producing sound. One entry per [`AppKey`].
    fn list_audio_apps(&mut self) -> AudioResult<Vec<AudioApp>>;

    /// Set the volume of every session of `app` (0.0..=1.0). Independent of the mute flag.
    fn set_app_volume(&mut self, app: &str, volume: f32) -> AudioResult<()>;

    /// Mute/unmute every session of `app`. Uses the OS mixer's own mute flag, so the
    /// user's volume level is preserved by the OS itself — nothing is lost if HushKey
    /// crashes while an app is muted.
    fn set_app_mute(&mut self, app: &str, muted: bool) -> AudioResult<()>;

    /// Mute every app that isn't already muted.
    fn mute_all(&mut self) -> AudioResult<MuteSnapshot> {
        let mut snap = MuteSnapshot::default();
        for app in self.list_audio_apps()? {
            if !app.muted {
                match self.set_app_mute(&app.key, true) {
                    Ok(()) => snap.muted_keys.push(app.key),
                    Err(e) => log::warn!("mute_all: {}: {e}", app.key),
                }
            }
        }
        Ok(snap)
    }

    /// Undo a [`mute_all`](Self::mute_all). Apps that have since exited are skipped.
    fn restore_all(&mut self, snapshot: &MuteSnapshot) -> AudioResult<()> {
        for key in &snapshot.muted_keys {
            match self.set_app_mute(key, false) {
                Ok(()) | Err(AudioError::NotRunning(_)) => {}
                Err(e) => log::warn!("restore_all: {key}: {e}"),
            }
        }
        Ok(())
    }

    /// Active output devices, default first is *not* guaranteed (check `is_default`).
    fn output_devices(&mut self) -> AudioResult<Vec<OutputDevice>> {
        Ok(Vec::new())
    }

    /// Make `id` the system default output device.
    fn set_default_output_device(&mut self, _id: &str) -> AudioResult<()> {
        Err(AudioError::Unsupported("switching the output device"))
    }

    /// Volume / mute of the default output device.
    fn master_volume(&mut self) -> AudioResult<MasterVolume> {
        Err(AudioError::Unsupported("system volume"))
    }

    fn set_master_volume(&mut self, _volume: f32) -> AudioResult<()> {
        Err(AudioError::Unsupported("system volume"))
    }

    fn set_master_mute(&mut self, _muted: bool) -> AudioResult<()> {
        Err(AudioError::Unsupported("system mute"))
    }

    /// Route every stream of `app` to `device`, or back to the system default with `None`.
    fn set_app_output_device(&mut self, _app: &str, _device: Option<&str>) -> AudioResult<()> {
        Err(AudioError::Unsupported("per-app output devices"))
    }

    /// Best-effort app icon. Default: none (UI shows a letter avatar).
    fn app_icon(&mut self, _app: &AudioApp) -> Option<IconData> {
        None
    }

    /// Short note for the UI about platform limitations, if any.
    fn platform_note(&self) -> Option<&'static str> {
        None
    }
}

/// Create the backend for the current OS. Must be called on the thread that will use it.
pub fn create_controller() -> AudioResult<Box<dyn AudioController>> {
    #[cfg(target_os = "windows")]
    return Ok(Box::new(windows::WasapiController::new()?));
    #[cfg(target_os = "macos")]
    return Ok(Box::new(macos::CoreAudioTapController::new()?));
    #[cfg(target_os = "linux")]
    return Ok(Box::new(linux::PulseController::new()?));
    #[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
    return Ok(Box::new(null::NullController::default()));
}
