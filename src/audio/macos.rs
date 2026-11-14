//! macOS backend: Core Audio **process taps** (macOS 14.2+).
//!
//! # Platform limitation — read before editing
//!
//! macOS has no public API to set another application's output volume. The options are:
//! 1. Install a virtual audio driver (what SoundSource / Background Music do) — needs a
//!    kernel/HAL plug-in install and admin rights. Rejected: too invasive for a hotkey tool.
//! 2. **Core Audio process taps** (`AudioHardwareCreateProcessTap`, macOS 14.2+). A tap
//!    created with `muteBehavior = CATapMuted` silences the tapped process for as long as
//!    the tap exists. ← what HushKey uses.
//!
//! Consequences:
//! - Per-app **mute works**; per-app **volume levels do not** (`per_app_volume: false`).
//!   `set_app_volume` accepts only 0.0 (mute) and 1.0 (unmute) and reports `Unsupported`
//!   for anything in between — it never fakes a level.
//! - Requires macOS 14.2+. On older systems the symbols are absent; we look them up with
//!   `dlsym` (never hard-link them, or the binary would fail to launch on macOS 13) and
//!   fall back to listing apps read-only.
//! - The user must grant the audio-capture permission ("Screen & System Audio Recording"
//!   in System Settings → Privacy & Security). The app bundle's Info.plist needs
//!   `NSAudioCaptureUsageDescription` (see docs/building.md).
//! - Crash-safe: taps belong to our process, so if HushKey exits or crashes every muted
//!   app becomes audible again automatically.
//!
//! **Untested on real hardware by the original author** (developed on Windows, compile-
//! checked for `aarch64-apple-darwin`). Reports from macOS users are very welcome.

use super::{AudioApp, AudioController, AudioError, AudioResult, Capabilities};
use objc2::msg_send;
use objc2::rc::Retained;
use objc2::runtime::{AnyClass, AnyObject};
use objc2_foundation::{NSArray, NSNumber, NSString};
use std::collections::{BTreeMap, HashMap};
use std::ffi::c_void;
use std::path::PathBuf;

type AudioObjectID = u32;
type OSStatus = i32;

#[repr(C)]
struct AudioObjectPropertyAddress {
    selector: u32,
    scope: u32,
    element: u32,
}

const fn fourcc(s: &[u8; 4]) -> u32 {
    ((s[0] as u32) << 24) | ((s[1] as u32) << 16) | ((s[2] as u32) << 8) | (s[3] as u32)
}

const K_AUDIO_OBJECT_SYSTEM_OBJECT: AudioObjectID = 1;
const K_SCOPE_GLOBAL: u32 = fourcc(b"glob");
const K_ELEMENT_MAIN: u32 = 0;
const K_HW_PROCESS_OBJECT_LIST: u32 = fourcc(b"prs#");
const K_PROCESS_PID: u32 = fourcc(b"ppid");
const K_PROCESS_BUNDLE_ID: u32 = fourcc(b"pbid");
const K_PROCESS_IS_RUNNING_OUTPUT: u32 = fourcc(b"piro");
/// `CATapMuteBehavior.CATapMuted`
const CA_TAP_MUTED: isize = 1;

#[link(name = "CoreAudio", kind = "framework")]
extern "C" {
    fn AudioObjectGetPropertyDataSize(
        id: AudioObjectID,
        addr: *const AudioObjectPropertyAddress,
        qualifier_size: u32,
        qualifier: *const c_void,
        out_size: *mut u32,
    ) -> OSStatus;
    fn AudioObjectGetPropertyData(
        id: AudioObjectID,
        addr: *const AudioObjectPropertyAddress,
        qualifier_size: u32,
        qualifier: *const c_void,
        io_size: *mut u32,
        out: *mut c_void,
    ) -> OSStatus;
}

type CreateTapFn = unsafe extern "C" fn(*mut AnyObject, *mut AudioObjectID) -> OSStatus;
type DestroyTapFn = unsafe extern "C" fn(AudioObjectID) -> OSStatus;

struct TapApi {
    create: CreateTapFn,
    destroy: DestroyTapFn,
    description_class: &'static AnyClass,
}

impl TapApi {
    /// Resolve the macOS 14.2+ tap API at runtime.
    fn load() -> Option<Self> {
        unsafe {
            let create = libc::dlsym(libc::RTLD_DEFAULT, c"AudioHardwareCreateProcessTap".as_ptr());
            let destroy = libc::dlsym(libc::RTLD_DEFAULT, c"AudioHardwareDestroyProcessTap".as_ptr());
            if create.is_null() || destroy.is_null() {
                return None;
            }
            Some(Self {
                create: std::mem::transmute::<*mut c_void, CreateTapFn>(create),
                destroy: std::mem::transmute::<*mut c_void, DestroyTapFn>(destroy),
                description_class: AnyClass::get(c"CATapDescription")?,
            })
        }
    }
}

struct Process {
    object: AudioObjectID,
    pid: u32,
    running_output: bool,
    key: String,
    name: String,
    path: Option<PathBuf>,
}

struct Tap {
    id: AudioObjectID,
    objects: Vec<AudioObjectID>,
}

pub struct CoreAudioTapController {
    api: Option<TapApi>,
    taps: HashMap<String, Tap>,
}

impl CoreAudioTapController {
    pub fn new() -> AudioResult<Self> {
        let api = TapApi::load();
        if api.is_none() {
            log::warn!("Core Audio process taps unavailable (needs macOS 14.2+); muting disabled");
        }
        Ok(Self { api, taps: HashMap::new() })
    }

    fn processes(&self) -> AudioResult<Vec<Process>> {
        let objects: Vec<AudioObjectID> = unsafe { get_array(K_AUDIO_OBJECT_SYSTEM_OBJECT, K_HW_PROCESS_OBJECT_LIST)? };
        let mut out = Vec::new();
        for object in objects {
            let pid = unsafe { get_scalar::<i32>(object, K_PROCESS_PID) }.unwrap_or(-1);
            if pid <= 0 || pid as u32 == std::process::id() {
                continue;
            }
            let running_output = unsafe { get_scalar::<u32>(object, K_PROCESS_IS_RUNNING_OUTPUT) }.unwrap_or(0) != 0;
            let bundle_id = unsafe { get_cfstring(object, K_PROCESS_BUNDLE_ID) }.unwrap_or_default();
            let path = pid_path(pid);
            let (key, name) = identify(path.as_ref(), &bundle_id);
            out.push(Process { object, pid: pid as u32, running_output, key, name, path });
        }
        Ok(out)
    }

    fn create_tap(&self, objects: &[AudioObjectID]) -> AudioResult<AudioObjectID> {
        let api = self.api.as_ref().ok_or(AudioError::Unsupported("per-app mute requires macOS 14.2 or newer"))?;
        unsafe {
            let numbers: Vec<Retained<NSNumber>> = objects.iter().map(|o| NSNumber::new_u32(*o)).collect();
            let array = NSArray::from_retained_slice(&numbers);
            let alloc: *mut AnyObject = msg_send![api.description_class, alloc];
            let desc: *mut AnyObject = msg_send![alloc, initStereoMixdownOfProcesses: &*array];
            if desc.is_null() {
                return Err(AudioError::Backend("CATapDescription init failed".into()));
            }
            let desc: Retained<AnyObject> = Retained::from_raw(desc).expect("non-null");
            let name = NSString::from_str("HushKey mute");
            let _: () = msg_send![&*desc, setName: &*name];
            let _: () = msg_send![&*desc, setPrivate: true];
            let _: () = msg_send![&*desc, setMuteBehavior: CA_TAP_MUTED];
            let mut tap: AudioObjectID = 0;
            let status = (api.create)(Retained::as_ptr(&desc) as *mut AnyObject, &mut tap);
            if status != 0 {
                return Err(AudioError::PermissionDenied(format!(
                    "macOS refused to create an audio tap (OSStatus {status}). Allow HushKey under System Settings → \
                     Privacy & Security → Screen & System Audio Recording."
                )));
            }
            Ok(tap)
        }
    }

    fn destroy_tap(&mut self, key: &str) {
        if let (Some(tap), Some(api)) = (self.taps.remove(key), self.api.as_ref()) {
            let status = unsafe { (api.destroy)(tap.id) };
            if status != 0 {
                log::warn!("AudioHardwareDestroyProcessTap({}) = {status}", tap.id);
            }
        }
    }
}

impl Drop for CoreAudioTapController {
    fn drop(&mut self) {
        let keys: Vec<String> = self.taps.keys().cloned().collect();
        for k in keys {
            self.destroy_tap(&k);
        }
    }
}

impl AudioController for CoreAudioTapController {
    fn backend_name(&self) -> &'static str {
        "macOS Core Audio process taps"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities { per_app_volume: false, per_app_mute: self.api.is_some() }
    }

    fn list_audio_apps(&mut self) -> AudioResult<Vec<AudioApp>> {
        let procs = self.processes()?;
        let mut grouped: BTreeMap<String, Vec<&Process>> = BTreeMap::new();
        for p in &procs {
            // Show real apps (inside an .app bundle), anything currently playing, and anything we muted.
            let is_app = p.path.as_ref().is_some_and(|p| p.to_string_lossy().contains(".app/"));
            if is_app || p.running_output || self.taps.contains_key(&p.key) {
                grouped.entry(p.key.clone()).or_default().push(p);
            }
        }

        // Drop taps for apps that quit; re-tap apps whose process set changed (new helper process).
        let stale: Vec<String> = self.taps.keys().filter(|k| !grouped.contains_key(*k)).cloned().collect();
        for k in stale {
            self.destroy_tap(&k);
        }
        let mut retap = Vec::new();
        for (key, ps) in &grouped {
            if let Some(tap) = self.taps.get(key) {
                let mut now: Vec<AudioObjectID> = ps.iter().map(|p| p.object).collect();
                now.sort_unstable();
                if now != tap.objects {
                    retap.push((key.clone(), now));
                }
            }
        }
        for (key, objects) in retap {
            self.destroy_tap(&key);
            if let Ok(id) = self.create_tap(&objects) {
                self.taps.insert(key, Tap { id, objects });
            }
        }

        let apps = grouped
            .into_iter()
            .map(|(key, ps)| AudioApp {
                muted: self.taps.contains_key(&key),
                name: ps[0].name.clone(),
                pids: ps.iter().map(|p| p.pid).collect(),
                volume: None, // not readable per-app on macOS
                exe_path: ps[0].path.clone(),
                key,
            })
            .collect::<Vec<_>>();
        let mut apps = apps;
        apps.sort_by_key(|a| a.name.to_lowercase());
        Ok(apps)
    }

    fn set_app_volume(&mut self, app: &str, volume: f32) -> AudioResult<()> {
        if volume <= 0.0 {
            self.set_app_mute(app, true)
        } else if volume >= 1.0 {
            self.set_app_mute(app, false)
        } else {
            Err(AudioError::Unsupported("per-app volume levels on macOS (mute/unmute only)"))
        }
    }

    fn set_app_mute(&mut self, app: &str, muted: bool) -> AudioResult<()> {
        let mut objects: Vec<AudioObjectID> =
            self.processes()?.into_iter().filter(|p| p.key == app).map(|p| p.object).collect();
        if objects.is_empty() {
            self.destroy_tap(app);
            return Err(AudioError::NotRunning(app.into()));
        }
        objects.sort_unstable();
        if muted {
            if !self.taps.contains_key(app) {
                let id = self.create_tap(&objects)?;
                self.taps.insert(app.to_owned(), Tap { id, objects });
            }
        } else {
            self.destroy_tap(app);
        }
        Ok(())
    }

    fn platform_note(&self) -> Option<&'static str> {
        Some(if self.api.is_some() {
            "macOS can mute individual apps but can't set their volume level. Muting needs the \
             Screen & System Audio Recording permission."
        } else {
            "Per-app muting needs macOS 14.2 or newer. Apps are listed read-only."
        })
    }
}

/// Key + display name. Helper processes (e.g. "Google Chrome Helper (Renderer).app" inside
/// "Google Chrome.app") resolve to the *outermost* bundle so they group under their app.
fn identify(path: Option<&PathBuf>, bundle_id: &str) -> (String, String) {
    if let Some(p) = path {
        let s = p.to_string_lossy();
        if let Some(end) = s.find(".app/") {
            let bundle_path = &s[..end];
            let name = bundle_path.rsplit('/').next().unwrap_or(bundle_path).to_owned();
            return (format!("{}.app", name.to_lowercase()), name);
        }
    }
    if !bundle_id.is_empty() {
        let name = bundle_id.rsplit('.').next().unwrap_or(bundle_id).to_owned();
        return (bundle_id.to_lowercase(), name);
    }
    let exe =
        path.and_then(|p| p.file_name()).map(|f| f.to_string_lossy().into_owned()).unwrap_or_else(|| "unknown".into());
    (exe.to_lowercase(), exe)
}

fn pid_path(pid: i32) -> Option<PathBuf> {
    let mut buf = vec![0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
    let n = unsafe { libc::proc_pidpath(pid, buf.as_mut_ptr() as *mut c_void, buf.len() as u32) };
    if n <= 0 {
        return None;
    }
    buf.truncate(n as usize);
    Some(PathBuf::from(String::from_utf8_lossy(&buf).into_owned()))
}

fn addr(selector: u32) -> AudioObjectPropertyAddress {
    AudioObjectPropertyAddress { selector, scope: K_SCOPE_GLOBAL, element: K_ELEMENT_MAIN }
}

unsafe fn get_array<T: Copy + Default>(object: AudioObjectID, selector: u32) -> AudioResult<Vec<T>> {
    let a = addr(selector);
    let mut size = 0u32;
    let status = AudioObjectGetPropertyDataSize(object, &a, 0, std::ptr::null(), &mut size);
    if status != 0 {
        return Err(AudioError::Backend(format!("AudioObjectGetPropertyDataSize = {status}")));
    }
    let mut out = vec![T::default(); size as usize / std::mem::size_of::<T>()];
    let status =
        AudioObjectGetPropertyData(object, &a, 0, std::ptr::null(), &mut size, out.as_mut_ptr() as *mut c_void);
    if status != 0 {
        return Err(AudioError::Backend(format!("AudioObjectGetPropertyData = {status}")));
    }
    out.truncate(size as usize / std::mem::size_of::<T>());
    Ok(out)
}

unsafe fn get_scalar<T: Copy + Default>(object: AudioObjectID, selector: u32) -> Option<T> {
    let a = addr(selector);
    let mut v = T::default();
    let mut size = std::mem::size_of::<T>() as u32;
    let status =
        AudioObjectGetPropertyData(object, &a, 0, std::ptr::null(), &mut size, &mut v as *mut T as *mut c_void);
    (status == 0).then_some(v)
}

/// Reads a `CFStringRef` property (returned +1 retained; toll-free bridged to `NSString`).
unsafe fn get_cfstring(object: AudioObjectID, selector: u32) -> Option<String> {
    let raw: *mut NSString = get_scalar::<*mut c_void>(object, selector)? as *mut NSString;
    let s: Retained<NSString> = Retained::from_raw(raw)?;
    Some(s.to_string())
}
