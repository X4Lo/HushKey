//! Small OS integrations: launch-at-login, opening folders, single-instance guard.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::Path;
use std::time::Duration;

pub const MINIMIZED_ARG: &str = "--minimized";

fn auto_launch() -> Result<auto_launch::AutoLaunch, String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let mut b = auto_launch::AutoLaunchBuilder::new();
    b.set_app_name("HushKey")
        .set_app_path(&exe.to_string_lossy())
        .set_args(&[MINIMIZED_ARG])
        .set_macos_launch_mode(auto_launch::MacOSLaunchMode::LaunchAgent);
    b.build().map_err(|e| e.to_string())
}

pub fn set_launch_on_startup(enabled: bool) -> Result<(), String> {
    let al = auto_launch()?;
    let currently = al.is_enabled().unwrap_or(false);
    match (enabled, currently) {
        (true, false) => al.enable().map_err(|e| e.to_string()),
        (false, true) => al.disable().map_err(|e| e.to_string()),
        _ => Ok(()),
    }
}

pub fn open_folder(path: &Path) {
    #[cfg(target_os = "windows")]
    let cmd = "explorer";
    #[cfg(target_os = "macos")]
    let cmd = "open";
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    let cmd = "xdg-open";
    let _ = std::fs::create_dir_all(path);
    if let Err(e) = std::process::Command::new(cmd).arg(path).spawn() {
        log::warn!("open {}: {e}", path.display());
    }
}

// ── Single instance ────────────────────────────────────────────────────────────
// A loopback-only socket doubles as lock and "show yourself" signal: launching HushKey
// again just brings the running window up. Loopback binds don't trigger firewall prompts.

const PORT: u16 = 47_219;
const MAGIC: &[u8] = b"HUSHKEY1\n";

pub enum Instance {
    /// We're the first instance; call `listen` once the event loop is ready.
    Primary(TcpListener),
    /// Another HushKey is running and has been asked to show its window.
    AlreadyRunning,
    /// The port is taken by something else; run without the guard.
    Unguarded,
}

pub fn acquire_single_instance() -> Instance {
    let addr = SocketAddr::from(([127, 0, 0, 1], PORT));
    match TcpListener::bind(addr) {
        Ok(l) => Instance::Primary(l),
        Err(_) => match TcpStream::connect_timeout(&addr, Duration::from_millis(300)) {
            Ok(mut s) => {
                let _ = s.set_read_timeout(Some(Duration::from_millis(500)));
                let mut buf = [0u8; 9];
                if s.read_exact(&mut buf).is_ok() && buf == MAGIC {
                    Instance::AlreadyRunning
                } else {
                    Instance::Unguarded
                }
            }
            Err(_) => Instance::Unguarded,
        },
    }
}

/// Accept "show" pings from later launches on a tiny background thread.
pub fn listen(listener: TcpListener, on_ping: impl Fn() + Send + 'static) {
    let _ = std::thread::Builder::new().name("hushkey-instance".into()).stack_size(64 * 1024).spawn(move || {
        for mut stream in listener.incoming().flatten() {
            let _ = stream.write_all(MAGIC);
            on_ping();
        }
    });
}
