// No console window on Windows release builds.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

//! HushKey — global hotkeys to mute individual apps, or everything at once.
//!
//! Module map (see docs/architecture.md):
//! - `audio`    platform-neutral `AudioController` trait + per-OS backends + worker thread
//! - `hotkeys`  accelerator parsing/capture + global registration
//! - `config`   TOML persistence
//! - `ui`       Slint components, tray, toast, procedural icon
//! - `app`      main-thread controller wiring it all together
//! - `platform` launch-at-login, single instance, open-folder

mod app;
mod audio;
mod config;
mod hotkeys;
mod platform;
mod ui;

slint::include_modules!();

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("hushkey=info")).init();

    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--list") {
        list_apps();
        return;
    }
    if args.iter().any(|a| a == "--help" || a == "-h") {
        println!(
            "HushKey {}\n\n  --minimized   start in the tray\n  --list        print detected audio apps and exit",
            env!("CARGO_PKG_VERSION")
        );
        return;
    }

    let listener = match platform::acquire_single_instance() {
        platform::Instance::AlreadyRunning => {
            log::info!("HushKey is already running; asked it to show its window");
            return;
        }
        platform::Instance::Primary(l) => Some(l),
        platform::Instance::Unguarded => {
            log::warn!("single-instance port busy; continuing without the guard");
            None
        }
    };

    if let Err(e) = select_backend() {
        eprintln!("HushKey: cannot initialise the UI backend: {e}");
        std::process::exit(1);
    }

    let start_hidden = args.iter().any(|a| a == platform::MINIMIZED_ARG) || {
        let store = config::ConfigStore::default_location();
        store.map(|s| s.load().settings.start_minimized).unwrap_or(false)
    };

    app::App::start(start_hidden);
    if let Some(l) = listener {
        platform::listen(l, || {
            let _ = slint::invoke_from_event_loop(|| {
                app::with_app(|a| a.show_window());
            });
        });
    }

    // Keep running with zero windows open: the tray and hotkeys live on.
    if let Err(e) = slint::run_event_loop_until_quit() {
        log::error!("event loop: {e}");
    }
    app::App::shutdown();
}

fn select_backend() -> Result<(), slint::PlatformError> {
    let selector = slint::BackendSelector::new()
        .backend_name("winit".into())
        .renderer_name("software".into())
        .with_winit_window_attributes_hook(ui::toast::window_attributes_hook);

    // Menu-bar app on macOS: no Dock icon.
    #[cfg(target_os = "macos")]
    let selector = selector.with_winit_event_loop_builder({
        use slint::winit_030::winit::platform::macos::{ActivationPolicy, EventLoopBuilderExtMacOS};
        let mut b = slint::winit_030::winit::event_loop::EventLoop::with_user_event();
        b.with_activation_policy(ActivationPolicy::Accessory);
        b
    });

    selector.select()
}

/// `hushkey --list`: diagnostic dump, handy for bug reports.
fn list_apps() {
    let mut ctl = match audio::create_controller() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("audio backend unavailable: {e}");
            std::process::exit(1);
        }
    };
    println!("backend: {}", ctl.backend_name());
    match ctl.list_audio_apps() {
        Ok(apps) => {
            for a in apps {
                let vol = a.volume.map(|v| format!("{:>3.0}%", v * 100.0)).unwrap_or_else(|| "  — ".into());
                println!("{:<32} {:<28} vol {vol}  muted {:<5}  pids {:?}", a.key, a.name, a.muted, a.pids);
            }
        }
        Err(e) => eprintln!("error: {e}"),
    }
}
