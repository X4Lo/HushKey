//! Windows backend: WASAPI audio sessions.
//!
//! Each app that opens an audio stream gets an *audio session* per output device
//! (the same thing the Windows Volume Mixer shows). We enumerate sessions on **every
//! active render device**, not just the default one, so apps routed to headphones
//! or a virtual device are found too. Sessions are grouped by executable name.
//!
//! Full per-app volume and mute are natively supported (`ISimpleAudioVolume`). System
//! volume uses `IAudioEndpointVolume` on the default device.
//!
//! # Undocumented interfaces
//! Windows has no *public* API to change the default output device or an app's output
//! device, yet both are what the Sound settings page does. HushKey uses the same private
//! COM interfaces as EarTrumpet, SoundSwitch and friends:
//! - `IPolicyConfig` (`CPolicyConfigClient`) → `SetDefaultEndpoint`, stable since Vista.
//! - `IAudioPolicyConfigFactory` (`Windows.Media.Internal.AudioPolicyConfig`) →
//!   `Set/GetPersistedDefaultAudioEndpoint`, Windows 10 1803+. Its IID changed in 21H2,
//!   so both are tried. If neither works the feature is reported unsupported instead of
//!   failing. Windows itself persists the per-app choice (by executable), like the
//!   "App volume and device preferences" page.
//!
//! Threading: this type must stay on the thread that created it (COM MTA init).

use super::{AudioApp, AudioController, AudioError, AudioResult, Capabilities, IconData, MasterVolume, OutputDevice};
use std::collections::{BTreeMap, HashMap};
use std::ffi::c_void;
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use windows::core::{Interface, GUID, HRESULT, HSTRING, PCWSTR, PWSTR};
use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
use windows::Win32::Foundation::{CloseHandle, S_OK};
use windows::Win32::Graphics::Gdi::{
    DeleteObject, GetDC, GetDIBits, GetObjectW, ReleaseDC, BITMAP, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS,
};
use windows::Win32::Media::Audio::Endpoints::IAudioEndpointVolume;
use windows::Win32::Media::Audio::{
    eConsole, eMultimedia, eRender, AudioSessionStateExpired, IAudioSessionControl2, IAudioSessionManager2, IMMDevice,
    IMMDeviceEnumerator, ISimpleAudioVolume, MMDeviceEnumerator, DEVICE_STATE_ACTIVE,
};
use windows::Win32::Storage::FileSystem::{GetFileVersionInfoSizeW, GetFileVersionInfoW, VerQueryValueW};
use windows::Win32::System::Com::StructuredStorage::PropVariantToStringAlloc;
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize, CLSCTX_ALL, COINIT_MULTITHREADED, STGM_READ,
};
use windows::Win32::System::Threading::{
    OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::UI::Shell::SHDefExtractIconW;
use windows::Win32::UI::WindowsAndMessaging::{DestroyIcon, GetIconInfo, HICON, ICONINFO};

const SYSTEM_SOUNDS_KEY: &str = "system-sounds";
const ICON_SIZE: u32 = 64;

// ── Private policy-config interfaces (see module docs) ─────────────────────────────
// Only the methods HushKey calls have real signatures; the others are vtable padding.

const CLSID_POLICY_CONFIG_CLIENT: GUID = GUID::from_u128(0x870af99c_171d_4f9e_af0d_e63df40c2bc9);

const AUDIO_POLICY_CONFIG_CLASS: &str = "Windows.Media.Internal.AudioPolicyConfig";
/// IID since Windows 10 21H2 (build 21390+), then the one used by 1803–21H1.
const AUDIO_POLICY_CONFIG_IIDS: [GUID; 2] =
    [GUID::from_u128(0xab3d4648_e242_459f_b02f_541c70306324), GUID::from_u128(0x2a59116d_6c4f_45e0_a74f_707e3fef9258)];
const MMDEVAPI_PREFIX: &str = r"\\?\SWD#MMDEVAPI#";
const RENDER_INTERFACE_SUFFIX: &str = "#{e6327cad-dcec-4949-ae8a-991e976a79d2}";

mod policy {
    #![allow(non_snake_case, dead_code)]
    use std::ffi::c_void;
    use windows::core::{IUnknown, IUnknown_Vtbl, HRESULT, PCWSTR};
    use windows::Win32::Media::Audio::{EDataFlow, ERole};

    #[windows::core::interface("f8679f50-850a-41cf-9c72-430f290290c8")]
    pub unsafe trait IPolicyConfig: IUnknown {
        pub fn GetMixFormat(&self) -> HRESULT;
        pub fn GetDeviceFormat(&self) -> HRESULT;
        pub fn ResetDeviceFormat(&self) -> HRESULT;
        pub fn SetDeviceFormat(&self) -> HRESULT;
        pub fn GetProcessingPeriod(&self) -> HRESULT;
        pub fn SetProcessingPeriod(&self) -> HRESULT;
        pub fn GetShareMode(&self) -> HRESULT;
        pub fn SetShareMode(&self) -> HRESULT;
        pub fn GetPropertyValue(&self) -> HRESULT;
        pub fn SetPropertyValue(&self) -> HRESULT;
        pub fn SetDefaultEndpoint(&self, device_id: PCWSTR, role: ERole) -> HRESULT;
        pub fn SetEndpointVisibility(&self) -> HRESULT;
    }

    // The IID here is only used by `cast()`, which HushKey never does: the factory is
    // obtained via `RoGetActivationFactory` with whichever IID the OS accepts (same layout).
    #[windows::core::interface("ab3d4648-e242-459f-b02f-541c70306324")]
    pub unsafe trait IAudioPolicyConfigFactory: IUnknown {
        // IInspectable
        pub fn GetIids(&self) -> HRESULT;
        pub fn GetRuntimeClassName(&self) -> HRESULT;
        pub fn GetTrustLevel(&self) -> HRESULT;
        pub fn add_CtxVolumeChange(&self) -> HRESULT;
        pub fn remove_CtxVolumeChanged(&self) -> HRESULT;
        pub fn add_RingerVibrateStateChanged(&self) -> HRESULT;
        pub fn remove_RingerVibrateStateChange(&self) -> HRESULT;
        pub fn SetVolumeGroupGainForId(&self) -> HRESULT;
        pub fn GetVolumeGroupGainForId(&self) -> HRESULT;
        pub fn GetActiveVolumeGroupForEndpointId(&self) -> HRESULT;
        pub fn GetVolumeGroupsForEndpoint(&self) -> HRESULT;
        pub fn GetCurrentVolumeContext(&self) -> HRESULT;
        pub fn SetVolumeGroupMuteForId(&self) -> HRESULT;
        pub fn GetVolumeGroupMuteForId(&self) -> HRESULT;
        pub fn SetRingerVibrateState(&self) -> HRESULT;
        pub fn GetRingerVibrateState(&self) -> HRESULT;
        pub fn SetPreferredChatApplication(&self) -> HRESULT;
        pub fn ResetPreferredChatApplication(&self) -> HRESULT;
        pub fn GetPreferredChatApplication(&self) -> HRESULT;
        pub fn GetCurrentChatApplications(&self) -> HRESULT;
        pub fn add_ChatContextChanged(&self) -> HRESULT;
        pub fn remove_ChatContextChanged(&self) -> HRESULT;
        // `device_id` is a borrowed HSTRING handle (null = back to the system default).
        pub fn SetPersistedDefaultAudioEndpoint(
            &self,
            pid: u32,
            flow: EDataFlow,
            role: ERole,
            device_id: *mut c_void,
        ) -> HRESULT;
        // Writes an owned HSTRING handle to `device_id` (null when the app follows the default).
        pub fn GetPersistedDefaultAudioEndpoint(
            &self,
            pid: u32,
            flow: EDataFlow,
            role: ERole,
            device_id: *mut *mut c_void,
        ) -> HRESULT;
        pub fn ClearAllPersistedApplicationDefaultEndpoints(&self) -> HRESULT;
    }
}
use policy::{IAudioPolicyConfigFactory, IPolicyConfig};

fn audio_policy_factory() -> Option<IAudioPolicyConfigFactory> {
    windows::core::link!("api-ms-win-core-winrt-l1-1-0.dll" "system" fn RoGetActivationFactory(class: *mut c_void, iid: *const GUID, factory: *mut *mut c_void) -> HRESULT);
    let class = HSTRING::from(AUDIO_POLICY_CONFIG_CLASS);
    for iid in &AUDIO_POLICY_CONFIG_IIDS {
        let mut raw: *mut c_void = std::ptr::null_mut();
        // SAFETY: HSTRING is a transparent handle; the factory comes back +1 and from_raw adopts it.
        let hr = unsafe { RoGetActivationFactory(std::mem::transmute_copy(&class), iid, &mut raw) };
        if hr.is_ok() && !raw.is_null() {
            return Some(unsafe { IAudioPolicyConfigFactory::from_raw(raw) });
        }
    }
    log::info!("per-app output devices unavailable (AudioPolicyConfig factory not found)");
    None
}

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
    device_names: HashMap<String, String>,
    policy: Option<IAudioPolicyConfigFactory>,
    _com: ComGuard,
}

impl WasapiController {
    pub fn new() -> AudioResult<Self> {
        unsafe {
            CoInitializeEx(None, COINIT_MULTITHREADED).ok().map_err(|e| err("CoInitializeEx", e))?;
            let guard = ComGuard;
            let enumerator: IMMDeviceEnumerator =
                CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).map_err(|e| err("MMDeviceEnumerator", e))?;
            Ok(Self {
                enumerator,
                procs: HashMap::new(),
                device_names: HashMap::new(),
                policy: audio_policy_factory(),
                _com: guard,
            })
        }
    }

    fn default_device(&self) -> AudioResult<IMMDevice> {
        unsafe { self.enumerator.GetDefaultAudioEndpoint(eRender, eMultimedia) }.map_err(|e| err("default device", e))
    }

    fn endpoint_volume(&self) -> AudioResult<IAudioEndpointVolume> {
        let device = self.default_device()?;
        unsafe { device.Activate::<IAudioEndpointVolume>(CLSCTX_ALL, None) }.map_err(|e| err("endpoint volume", e))
    }

    fn device_name(&mut self, device: &IMMDevice, id: &str) -> String {
        if let Some(n) = self.device_names.get(id) {
            return n.clone();
        }
        let name = unsafe {
            device
                .OpenPropertyStore(STGM_READ)
                .and_then(|store| store.GetValue(&PKEY_Device_FriendlyName))
                .and_then(|value| PropVariantToStringAlloc(&value))
                .ok()
                .map(take_pwstr)
        }
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| "Unknown device".to_owned());
        self.device_names.insert(id.to_owned(), name.clone());
        name
    }

    /// The device an app is explicitly routed to, from the first of its processes that has one.
    fn persisted_device(&self, pids: &[u32]) -> Option<String> {
        let policy = self.policy.as_ref()?;
        pids.iter().filter(|p| **p != 0).find_map(|&pid| unsafe {
            let mut raw: *mut c_void = std::ptr::null_mut();
            policy.GetPersistedDefaultAudioEndpoint(pid, eRender, eMultimedia, &mut raw).ok().ok()?;
            // SAFETY: an owned HSTRING handle (or null = empty); HSTRING frees it on drop.
            let s: HSTRING = std::mem::transmute::<*mut c_void, HSTRING>(raw);
            unpack_device_id(&s.to_string_lossy())
        })
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
        Capabilities {
            per_app_volume: true,
            per_app_mute: true,
            per_app_device: self.policy.is_some(),
            master_volume: true,
            output_devices: true,
        }
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
                let system = key == SYSTEM_SOUNDS_KEY;
                let (name, exe_path) = if system {
                    ("System Sounds".to_owned(), None)
                } else {
                    let info = pids.iter().find_map(|p| self.procs.get(p)).cloned();
                    info.map(|i| (i.name, i.path)).unwrap_or_else(|| (key.clone(), None))
                };
                let volume = Some(vols.iter().sum::<f32>() / vols.len().max(1) as f32);
                let device = if system { None } else { self.persisted_device(&pids) };
                let routable = !system && self.policy.is_some();
                AudioApp { key, name, pids, volume, muted, exe_path, device, routable }
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

    fn output_devices(&mut self) -> AudioResult<Vec<OutputDevice>> {
        let default_id = self.default_device().ok().and_then(|d| unsafe { d.GetId() }.ok()).map(take_pwstr);
        let mut out = Vec::new();
        unsafe {
            let devices = self
                .enumerator
                .EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE)
                .map_err(|e| err("EnumAudioEndpoints", e))?;
            for i in 0..devices.GetCount().map_err(|e| err("GetCount", e))? {
                let Ok(device) = devices.Item(i) else { continue };
                let Ok(id) = device.GetId().map(take_pwstr) else { continue };
                let name = self.device_name(&device, &id);
                out.push(OutputDevice { is_default: default_id.as_deref() == Some(id.as_str()), id, name });
            }
        }
        out.sort_by_key(|d| d.name.to_lowercase());
        Ok(out)
    }

    fn set_default_output_device(&mut self, id: &str) -> AudioResult<()> {
        unsafe {
            let policy: IPolicyConfig =
                CoCreateInstance(&CLSID_POLICY_CONFIG_CLIENT, None, CLSCTX_ALL).map_err(|e| err("PolicyConfig", e))?;
            let wide_id = HSTRING::from(id);
            for role in [eConsole, eMultimedia] {
                policy
                    .SetDefaultEndpoint(PCWSTR(wide_id.as_ptr()), role)
                    .ok()
                    .map_err(|e| err("SetDefaultEndpoint", e))?;
            }
        }
        Ok(())
    }

    fn master_volume(&mut self) -> AudioResult<MasterVolume> {
        let ep = self.endpoint_volume()?;
        unsafe {
            Ok(MasterVolume {
                volume: ep.GetMasterVolumeLevelScalar().ok(),
                muted: ep.GetMute().map(|b| b.as_bool()).unwrap_or(false),
            })
        }
    }

    fn set_master_volume(&mut self, volume: f32) -> AudioResult<()> {
        let ep = self.endpoint_volume()?;
        unsafe { ep.SetMasterVolumeLevelScalar(volume.clamp(0.0, 1.0), std::ptr::null()) }
            .map_err(|e| err("system volume", e))
    }

    fn set_master_mute(&mut self, muted: bool) -> AudioResult<()> {
        let ep = self.endpoint_volume()?;
        unsafe { ep.SetMute(muted, std::ptr::null()) }.map_err(|e| err("system mute", e))
    }

    fn set_app_output_device(&mut self, app: &str, device: Option<&str>) -> AudioResult<()> {
        if app == SYSTEM_SOUNDS_KEY {
            return Err(AudioError::Unsupported("routing system sounds"));
        }
        let Some(policy) = self.policy.clone() else {
            return Err(AudioError::Unsupported("per-app output devices (needs Windows 10 1803 or newer)"));
        };
        let mut pids: Vec<u32> = self.sessions()?.into_iter().filter(|s| s.key == app).map(|s| s.pid).collect();
        pids.sort_unstable();
        pids.dedup();
        if pids.is_empty() {
            return Err(AudioError::NotRunning(app.to_owned()));
        }
        let packed = device.map(|id| HSTRING::from(format!("{MMDEVAPI_PREFIX}{id}{RENDER_INTERFACE_SUFFIX}")));
        // SAFETY: a borrowed handle that outlives the calls; null resets the app to the default.
        let raw: *mut c_void =
            packed.as_ref().map(|h| unsafe { std::mem::transmute_copy(h) }).unwrap_or(std::ptr::null_mut());
        for pid in pids {
            for role in [eConsole, eMultimedia] {
                unsafe { policy.SetPersistedDefaultAudioEndpoint(pid, eRender, role, raw) }
                    .ok()
                    .map_err(|e| err("SetPersistedDefaultAudioEndpoint", e))?;
            }
        }
        Ok(())
    }

    fn app_icon(&mut self, app: &AudioApp) -> Option<IconData> {
        exe_icon(app.exe_path.as_deref()?)
    }
}

/// Copy a CoTaskMem-allocated wide string and free it.
fn take_pwstr(p: PWSTR) -> String {
    if p.is_null() {
        return String::new();
    }
    unsafe {
        let s = p.to_string().unwrap_or_default();
        CoTaskMemFree(Some(p.0 as *const c_void));
        s
    }
}

/// `\\?\SWD#MMDEVAPI#{0.0.0.00000000}.{guid}#{e6327cad-…}` → `{0.0.0.00000000}.{guid}`.
fn unpack_device_id(s: &str) -> Option<String> {
    let s = s.strip_prefix(MMDEVAPI_PREFIX).unwrap_or(s);
    let s = s.strip_suffix(RENDER_INTERFACE_SUFFIX).unwrap_or(s);
    (!s.is_empty()).then(|| s.to_owned())
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_id_round_trip() {
        let id = "{0.0.0.00000000}.{5c6a2d1e-7a3b-4c1d-9e2f-1a2b3c4d5e6f}";
        let packed = format!("{MMDEVAPI_PREFIX}{id}{RENDER_INTERFACE_SUFFIX}");
        assert_eq!(unpack_device_id(&packed).as_deref(), Some(id));
        assert_eq!(unpack_device_id(""), None);
    }
}
