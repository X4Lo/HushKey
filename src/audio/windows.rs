//! Windows backend: WASAPI audio sessions.
//!
//! Each app that opens an audio stream gets an *audio session* per output device
//! (the same thing the Windows Volume Mixer shows). We enumerate sessions on **every
//! active render device**, not just the default one, so apps routed to headphones
//! or a virtual device are found too. Sessions are grouped by executable name.
//!
//! Full per-app volume and mute are natively supported (`ISimpleAudioVolume`).
//! Threading: this type must stay on the thread that created it (COM MTA init).

use super::{AudioApp, AudioController, AudioError, AudioResult, Capabilities, IconData};
use std::collections::{BTreeMap, HashMap};
use std::ffi::c_void;
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use windows::core::{Interface, HSTRING, PCWSTR, PWSTR};
use windows::Win32::Foundation::{CloseHandle, S_OK};
use windows::Win32::Graphics::Gdi::{
    DeleteObject, GetDC, GetDIBits, GetObjectW, ReleaseDC, BITMAP, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS,
};
use windows::Win32::Media::Audio::{
    eRender, AudioSessionStateExpired, IAudioSessionControl2, IAudioSessionManager2, IMMDeviceEnumerator,
    ISimpleAudioVolume, MMDeviceEnumerator, DEVICE_STATE_ACTIVE,
};
use windows::Win32::Storage::FileSystem::{GetFileVersionInfoSizeW, GetFileVersionInfoW, VerQueryValueW};
use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_ALL, COINIT_MULTITHREADED};
use windows::Win32::System::Threading::{
    OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::UI::Shell::SHDefExtractIconW;
use windows::Win32::UI::WindowsAndMessaging::{DestroyIcon, GetIconInfo, HICON, ICONINFO};

const SYSTEM_SOUNDS_KEY: &str = "system-sounds";
const ICON_SIZE: u32 = 64;

fn err(context: &str, e: windows::core::Error) -> AudioError {
    AudioError::Backend(format!("{context}: {e}"))
}

#[derive(Clone)]
struct ProcInfo {
    key: String,
    name: String,
    path: Option<PathBuf>,
}

struct Session {
    key: String,
    pid: u32,
    volume: ISimpleAudioVolume,
}

/// Balances `CoInitializeEx`. Declared as the *last* field so COM objects are released first.
struct ComGuard;
impl Drop for ComGuard {
    fn drop(&mut self) {
        unsafe { CoUninitialize() };
    }
}

pub struct WasapiController {
    enumerator: IMMDeviceEnumerator,
    procs: HashMap<u32, ProcInfo>,
    _com: ComGuard,
}

impl WasapiController {
    pub fn new() -> AudioResult<Self> {
        unsafe {
            CoInitializeEx(None, COINIT_MULTITHREADED).ok().map_err(|e| err("CoInitializeEx", e))?;
            let guard = ComGuard;
            let enumerator: IMMDeviceEnumerator =
                CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).map_err(|e| err("MMDeviceEnumerator", e))?;
            Ok(Self { enumerator, procs: HashMap::new(), _com: guard })
        }
    }

    fn proc_info(&mut self, pid: u32) -> ProcInfo {
        if let Some(p) = self.procs.get(&pid) {
            return p.clone();
        }
        if self.procs.len() > 512 {
            self.procs.clear(); // PIDs get reused; keep the cache from growing forever.
        }
        let info = match process_path(pid) {
            Some(path) => {
                let file = path.file_name().map(|f| f.to_string_lossy().to_lowercase()).unwrap_or_default();
                let name = file_description(&path).unwrap_or_else(|| pretty_stem(&path));
                ProcInfo { key: file, name, path: Some(path) }
            }
            // Protected/elevated processes we can't open: still controllable by PID.
            None => ProcInfo { key: format!("pid-{pid}"), name: format!("Process {pid}"), path: None },
        };
        self.procs.insert(pid, info.clone());
        info
    }

    /// Every live audio session on every active output device.
    fn sessions(&mut self) -> AudioResult<Vec<Session>> {
        let mut out = Vec::new();
        unsafe {
            let devices = self
                .enumerator
                .EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE)
                .map_err(|e| err("EnumAudioEndpoints", e))?;
            let count = devices.GetCount().map_err(|e| err("GetCount", e))?;
            for i in 0..count {
                let Ok(device) = devices.Item(i) else { continue };
                let Ok(manager) = device.Activate::<IAudioSessionManager2>(CLSCTX_ALL, None) else { continue };
                let Ok(list) = manager.GetSessionEnumerator() else { continue };
                let n = list.GetCount().unwrap_or(0);
                for j in 0..n {
                    let Ok(control) = list.GetSession(j) else { continue };
                    if control.GetState().map(|s| s == AudioSessionStateExpired).unwrap_or(true) {
                        continue;
                    }
                    let Ok(control2) = control.cast::<IAudioSessionControl2>() else { continue };
                    let Ok(volume) = control.cast::<ISimpleAudioVolume>() else { continue };
                    let pid = control2.GetProcessId().unwrap_or(0);
                    let key = if control2.IsSystemSoundsSession() == S_OK || pid == 0 {
                        SYSTEM_SOUNDS_KEY.to_owned()
                    } else {
                        self.proc_info(pid).key
                    };
                    out.push(Session { key, pid, volume });
                }
            }
        }
        Ok(out)
    }

    fn for_each_session_of(
        &mut self,
        app: &str,
        f: impl Fn(&ISimpleAudioVolume) -> windows::core::Result<()>,
    ) -> AudioResult<()> {
        let sessions = self.sessions()?;
        let mut found = false;
        let mut last_err = None;
        for s in sessions.iter().filter(|s| s.key == app) {
            found = true;
            if let Err(e) = f(&s.volume) {
                last_err = Some(e);
            }
        }
        if !found {
            return Err(AudioError::NotRunning(app.to_owned()));
        }
        match last_err {
            Some(e) => Err(err(app, e)),
            None => Ok(()),
        }
    }
}

impl AudioController for WasapiController {
    fn backend_name(&self) -> &'static str {
        "Windows Core Audio (WASAPI)"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities { per_app_volume: true, per_app_mute: true }
    }

    fn list_audio_apps(&mut self) -> AudioResult<Vec<AudioApp>> {
        let sessions = self.sessions()?;
        // key -> (pids, volumes, all_muted)
        let mut grouped: BTreeMap<String, (Vec<u32>, Vec<f32>, bool)> = BTreeMap::new();
        for s in &sessions {
            let (vol, muted) = unsafe {
                (s.volume.GetMasterVolume().unwrap_or(1.0), s.volume.GetMute().map(|b| b.as_bool()).unwrap_or(false))
            };
            let e = grouped.entry(s.key.clone()).or_insert_with(|| (Vec::new(), Vec::new(), true));
            if !e.0.contains(&s.pid) {
                e.0.push(s.pid);
            }
            e.1.push(vol);
            e.2 &= muted;
        }
        let mut apps: Vec<AudioApp> = grouped
            .into_iter()
            .map(|(key, (pids, vols, muted))| {
                let (name, exe_path) = if key == SYSTEM_SOUNDS_KEY {
                    ("System Sounds".to_owned(), None)
                } else {
                    let info = pids.iter().find_map(|p| self.procs.get(p)).cloned();
                    info.map(|i| (i.name, i.path)).unwrap_or_else(|| (key.clone(), None))
                };
                let volume = Some(vols.iter().sum::<f32>() / vols.len().max(1) as f32);
                AudioApp { key, name, pids, volume, muted, exe_path }
            })
            .collect();
        apps.sort_by_key(|a| a.name.to_lowercase());
        Ok(apps)
    }

    fn set_app_volume(&mut self, app: &str, volume: f32) -> AudioResult<()> {
        let v = volume.clamp(0.0, 1.0);
        self.for_each_session_of(app, |s| unsafe { s.SetMasterVolume(v, std::ptr::null()) })
    }

    fn set_app_mute(&mut self, app: &str, muted: bool) -> AudioResult<()> {
        self.for_each_session_of(app, |s| unsafe { s.SetMute(muted, std::ptr::null()) })
    }

    fn app_icon(&mut self, app: &AudioApp) -> Option<IconData> {
        exe_icon(app.exe_path.as_deref()?)
    }
}

fn wide(path: &Path) -> Vec<u16> {
    path.as_os_str().encode_wide().chain(std::iter::once(0)).collect()
}

fn process_path(pid: u32) -> Option<PathBuf> {
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut buf = [0u16; 1024];
        let mut len = buf.len() as u32;
        let ok = QueryFullProcessImageNameW(handle, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut len);
        let _ = CloseHandle(handle);
        ok.ok()?;
        Some(PathBuf::from(String::from_utf16_lossy(&buf[..len as usize])))
    }
}

fn pretty_stem(path: &Path) -> String {
    let stem = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let mut c = stem.chars();
    match c.next() {
        Some(f) => f.to_uppercase().chain(c).collect(),
        None => stem,
    }
}

/// `FileDescription` from the exe's version resource ("Spotify", "Google Chrome").
fn file_description(path: &Path) -> Option<String> {
    let w = wide(path);
    unsafe {
        let size = GetFileVersionInfoSizeW(PCWSTR(w.as_ptr()), None);
        if size == 0 {
            return None;
        }
        let mut data = vec![0u8; size as usize];
        GetFileVersionInfoW(PCWSTR(w.as_ptr()), None, size, data.as_mut_ptr() as *mut c_void).ok()?;

        let mut ptr: *mut c_void = std::ptr::null_mut();
        let mut len = 0u32;
        if !VerQueryValueW(
            data.as_ptr() as *const c_void,
            &HSTRING::from("\\VarFileInfo\\Translation"),
            &mut ptr,
            &mut len,
        )
        .as_bool()
            || len < 4
        {
            return None;
        }
        let lang = *(ptr as *const u16);
        let codepage = *(ptr as *const u16).add(1);
        let sub = HSTRING::from(format!("\\StringFileInfo\\{lang:04x}{codepage:04x}\\FileDescription"));
        if !VerQueryValueW(data.as_ptr() as *const c_void, &sub, &mut ptr, &mut len).as_bool() || len == 0 {
            return None;
        }
        let chars = std::slice::from_raw_parts(ptr as *const u16, len as usize);
        let s = String::from_utf16_lossy(chars).trim_end_matches('\0').trim().to_owned();
        (!s.is_empty()).then_some(s)
    }
}

fn exe_icon(path: &Path) -> Option<IconData> {
    let w = wide(path);
    unsafe {
        let mut icon = HICON::default();
        SHDefExtractIconW(PCWSTR(w.as_ptr()), 0, 0, Some(&mut icon), None, ICON_SIZE).ok().ok()?;
        if icon.is_invalid() {
            return None;
        }
        let data = hicon_to_rgba(icon);
        let _ = DestroyIcon(icon);
        data
    }
}

unsafe fn hicon_to_rgba(icon: HICON) -> Option<IconData> {
    let mut info = ICONINFO::default();
    GetIconInfo(icon, &mut info).ok()?;
    let cleanup = |info: &ICONINFO| {
        if !info.hbmColor.is_invalid() {
            let _ = DeleteObject(info.hbmColor.into());
        }
        if !info.hbmMask.is_invalid() {
            let _ = DeleteObject(info.hbmMask.into());
        }
    };
    if info.hbmColor.is_invalid() {
        cleanup(&info);
        return None; // monochrome icon; letter avatar looks better anyway
    }
    let mut bmp = BITMAP::default();
    if GetObjectW(info.hbmColor.into(), std::mem::size_of::<BITMAP>() as i32, Some(&mut bmp as *mut _ as *mut c_void))
        == 0
    {
        cleanup(&info);
        return None;
    }
    let (w, h) = (bmp.bmWidth, bmp.bmHeight);
    let mut bi = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: w,
            biHeight: -h, // top-down
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut buf = vec![0u8; (w * h * 4) as usize];
    let dc = GetDC(None);
    let lines =
        GetDIBits(dc, info.hbmColor, 0, h as u32, Some(buf.as_mut_ptr() as *mut c_void), &mut bi, DIB_RGB_COLORS);
    ReleaseDC(None, dc);
    cleanup(&info);
    if lines == 0 {
        return None;
    }
    let has_alpha = buf.chunks_exact(4).any(|p| p[3] != 0);
    for px in buf.chunks_exact_mut(4) {
        px.swap(0, 2); // BGRA -> RGBA
        if !has_alpha {
            px[3] = 255;
        }
    }
    Some(IconData { width: w as u32, height: h as u32, rgba: buf })
}
