//! Application controller. Lives on the main (Slint event-loop) thread and glues the
//! UI, config, hotkeys, tray and the audio worker together.
//!
//! Memory strategy: the main window is *destroyed* when closed (not just hidden) and
//! rebuilt on demand from the state kept here, so the idle tray app holds no window
//! surfaces, render buffers or UI trees.

use crate::audio::worker::{self, AudioCommand, AudioEvent, AudioHandle};
use crate::audio::{AudioApp, Capabilities};
use crate::config::{Config, ConfigStore, Theme};
use crate::hotkeys::{capture_from_key_event, Accelerator, Capture, HotkeyAction, Hotkeys};
use crate::platform;
use crate::ui::icon::{self, IconState};
use crate::ui::toast::ToastPresenter;
use crate::ui::tray::{Tray, TrayCommand};
use crate::{AppRow, MainWindow};
use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::time::Duration;

const REFRESH_EVERY: Duration = Duration::from_millis(1500);
const TARGET_ALL: &str = "@all";
const TARGET_RESTORE: &str = "@restore";

thread_local! {
    static APP: RefCell<Option<Rc<App>>> = const { RefCell::new(None) };
}

/// Run `f` with the app, if it exists. Safe to call from any event-loop callback.
pub fn with_app<R>(f: impl FnOnce(&Rc<App>) -> R) -> Option<R> {
    let app = APP.with(|a| a.borrow().clone());
    app.map(|a| f(&a))
}

pub fn with_toast(f: impl FnOnce(&mut ToastPresenter)) {
    with_app(|a| f(&mut a.toast.borrow_mut()));
}

pub struct App {
    cfg: RefCell<Config>,
    store: ConfigStore,
    audio: RefCell<Option<AudioHandle>>,
    hotkeys: RefCell<Option<Hotkeys>>,
    tray: RefCell<Option<Tray>>,
    toast: RefCell<ToastPresenter>,
    window: RefCell<Option<MainWindow>>,
    rows: Rc<VecModel<AppRow>>,
    apps: RefCell<Vec<AudioApp>>,
    icons: RefCell<HashMap<String, slint::Image>>,
    all_muted: Cell<bool>,
    caps: Cell<Capabilities>,
    backend: RefCell<String>,
    platform_note: RefCell<String>,
    capturing: RefCell<Option<String>>,
    status: RefCell<(String, bool)>,
    refresh_timer: slint::Timer,
}

impl App {
    pub fn start(start_hidden: bool) -> Rc<App> {
        let store = ConfigStore::default_location().unwrap_or_else(|| ConfigStore::new("hushkey.toml".into()));
        let cfg = store.load();
        log::info!("config: {}", store.path().display());

        let audio = worker::spawn(|ev| {
            let _ = slint::invoke_from_event_loop(move || {
                with_app(|a| a.on_audio_event(ev));
            });
        });

        // Hot path: OS hotkey callback → audio thread, no UI hop.
        let tx = audio.sender();
        let hotkeys = match Hotkeys::new(move |action| {
            let cmd = match action {
                HotkeyAction::ToggleApp(key) => AudioCommand::ToggleApp { key, from_hotkey: true },
                HotkeyAction::ToggleAll => AudioCommand::ToggleAll { from_hotkey: true },
                HotkeyAction::RestoreAll => AudioCommand::RestoreAll { from_hotkey: true },
            };
            let _ = tx.send(cmd);
        }) {
            Ok(h) => Some(h),
            Err(e) => {
                log::error!("global hotkeys unavailable: {e}");
                None
            }
        };

        let app = Rc::new(App {
            cfg: RefCell::new(cfg),
            store,
            audio: RefCell::new(Some(audio)),
            hotkeys: RefCell::new(hotkeys),
            tray: RefCell::new(None),
            toast: RefCell::new(ToastPresenter::new()),
            window: RefCell::new(None),
            rows: Rc::new(VecModel::default()),
            apps: RefCell::default(),
            icons: RefCell::default(),
            all_muted: Cell::new(false),
            caps: Cell::new(Capabilities { per_app_volume: true, per_app_mute: true }),
            backend: RefCell::new("starting…".into()),
            platform_note: RefCell::default(),
            capturing: RefCell::new(None),
            status: RefCell::default(),
            refresh_timer: slint::Timer::default(),
        });
        APP.with(|a| *a.borrow_mut() = Some(app.clone()));

        if app.hotkeys.borrow().is_none() {
            app.set_status("Global shortcuts are unavailable on this system. See the README.", true);
        } else {
            app.apply_hotkeys();
        }
        // Registration "succeeds" through XWayland but never sees native Wayland focus,
        // so warn explicitly instead of failing silently.
        #[cfg(target_os = "linux")]
        if std::env::var("XDG_SESSION_TYPE").is_ok_and(|v| v.eq_ignore_ascii_case("wayland")) {
            app.set_status(
                "Wayland session: global shortcuts only fire while an X11/XWayland window is focused.                  Log into an X11 session for full support (see docs/building.md).",
                true,
            );
        }
        if app.cfg.borrow().settings.launch_on_startup {
            // Re-register so the entry follows the exe if it was moved/updated.
            let _ = platform::set_launch_on_startup(true);
        }

        // Tray must be created once the event loop runs (required on macOS).
        slint::Timer::single_shot(Duration::ZERO, || {
            with_app(|a| a.init_tray());
        });
        if !start_hidden {
            app.show_window();
        }
        app.send(AudioCommand::Refresh);
        app
    }

    /// Tear down in a safe order. Restores anything Mute All silenced.
    pub fn shutdown() {
        let Some(app) = APP.with(|a| a.borrow_mut().take()) else { return };
        app.refresh_timer.stop();
        app.window.borrow_mut().take();
        app.tray.borrow_mut().take();
        app.hotkeys.borrow_mut().take();
        let audio = app.audio.borrow_mut().take();
        if let Some(audio) = audio {
            audio.shutdown();
        }
    }

    fn send(&self, cmd: AudioCommand) {
        if let Some(a) = self.audio.borrow().as_ref() {
            a.send(cmd);
        }
    }

    // ── Tray ───────────────────────────────────────────────────────────────────

    fn init_tray(&self) {
        match Tray::new(|cmd| {
            with_app(|a| match cmd {
                TrayCommand::Open => a.show_window(),
                TrayCommand::ToggleAll => a.send(AudioCommand::ToggleAll { from_hotkey: false }),
                TrayCommand::Quit => {
                    let _ = slint::quit_event_loop();
                }
            });
        }) {
            Ok(mut t) => {
                t.set_all_muted(self.all_muted.get());
                *self.tray.borrow_mut() = Some(t);
            }
            Err(e) => {
                log::error!("tray icon unavailable: {e}");
                // Without a tray the user would have no way back in: keep the window up.
                self.show_window();
                self.set_status(
                    "No system tray found — closing this window will keep HushKey running in the background.",
                    true,
                );
            }
        }
    }

    // ── Window lifecycle ───────────────────────────────────────────────────────

    pub fn show_window(&self) {
        if self.window.borrow().is_none() {
            match self.build_window() {
                Ok(w) => *self.window.borrow_mut() = Some(w),
                Err(e) => {
                    log::error!("cannot create window: {e}");
                    return;
                }
            }
        }
        if let Some(w) = self.window.borrow().as_ref() {
            self.push_all(w);
            let _ = w.show();
            use slint::winit_030::WinitWindowAccessor;
            let dark = self.cfg.borrow().settings.theme == Theme::Dark;
            w.window().with_winit_window(|ww| {
                apply_native_chrome(ww, dark);
                ww.set_minimized(false);
                ww.focus_window();
            });
        }
        self.send(AudioCommand::Refresh);
        self.refresh_timer.start(slint::TimerMode::Repeated, REFRESH_EVERY, || {
            with_app(|a| a.send(AudioCommand::Refresh));
        });
    }

    /// Destroy the window (called after it was closed) to give the memory back.
    fn release_window(&self) {
        if self.capturing.borrow().is_some() {
            self.end_capture();
        }
        self.refresh_timer.stop();
        if let Some(w) = self.window.borrow_mut().take() {
            let _ = w.hide();
        }
    }

    fn build_window(&self) -> Result<MainWindow, slint::PlatformError> {
        let w = MainWindow::new()?;
        w.set_app_icon(crate::ui::icon_image(64, IconState::Normal));
        w.set_version(env!("CARGO_PKG_VERSION").into());
        w.set_config_path(self.store.path().display().to_string().into());
        w.set_apps(ModelRc::from(self.rows.clone()));

        w.window().on_close_requested(|| {
            // Can't drop the component from inside its own callback; do it next tick.
            slint::Timer::single_shot(Duration::ZERO, || {
                with_app(|a| a.release_window());
            });
            slint::CloseRequestResponse::HideWindow
        });

        w.on_start_capture(|t| {
            with_app(|a| a.start_capture(t.as_str()));
        });
        w.on_key_captured(|text, ctrl, alt, shift, meta| {
            with_app(|a| a.key_captured(text.as_str(), ctrl, alt, shift, meta));
        });
        w.on_cancel_capture(|| {
            with_app(|a| a.end_capture());
        });
        w.on_clear_shortcut(|t| {
            with_app(|a| a.set_shortcut(t.as_str(), None));
        });
        w.on_toggle_app(|key| {
            with_app(|a| a.send(AudioCommand::ToggleApp { key: key.to_string(), from_hotkey: false }));
        });
        w.on_toggle_all(|| {
            with_app(|a| a.send(AudioCommand::ToggleAll { from_hotkey: false }));
        });
        w.on_refresh(|| {
            with_app(|a| a.send(AudioCommand::Refresh));
        });
        w.on_dismiss_status(|| {
            with_app(|a| a.set_status("", false));
        });
        w.on_open_config_dir(|| {
            with_app(|a| {
                if let Some(dir) = a.store.path().parent() {
                    platform::open_folder(dir);
                }
            });
        });
        w.on_set_launch_on_startup(|v| {
            with_app(|a| match platform::set_launch_on_startup(v) {
                Ok(()) => a.update_settings(|s| s.launch_on_startup = v),
                Err(e) => {
                    a.set_status(&format!("Couldn't change launch-at-login: {e}"), true);
                    a.push_settings();
                }
            });
        });
        w.on_set_start_minimized(|v| {
            with_app(|a| a.update_settings(|s| s.start_minimized = v));
        });
        w.on_set_show_toast(|v| {
            with_app(|a| a.update_settings(|s| s.show_toast = v));
        });
        w.on_set_dark(|dark| {
            with_app(|a| a.update_settings(|s| s.theme = if dark { Theme::Dark } else { Theme::Light }));
        });
        Ok(w)
    }

    fn ui(&self) -> Option<MainWindow> {
        self.window.borrow().as_ref().map(|w| w.clone_strong())
    }

    fn push_all(&self, w: &MainWindow) {
        w.set_backend(self.backend.borrow().as_str().into());
        w.set_platform_note(self.platform_note.borrow().as_str().into());
        w.set_show_volume(self.caps.get().per_app_volume);
        w.set_all_muted(self.all_muted.get());
        let (msg, err) = self.status.borrow().clone();
        w.set_status(msg.into());
        w.set_status_is_error(err);
        w.set_capturing(self.capturing.borrow().clone().unwrap_or_default().into());
        w.set_hotkey_hint("Press a combo · Esc cancels · Backspace clears".into());
        self.push_settings_to(w);
        self.push_shortcuts_to(w);
        self.sync_rows();
    }

    fn push_settings(&self) {
        if let Some(w) = self.ui() {
            self.push_settings_to(&w);
        }
    }

    fn push_settings_to(&self, w: &MainWindow) {
        let s = &self.cfg.borrow().settings;
        let dark = s.theme == Theme::Dark;
        w.set_dark(dark);
        use slint::winit_030::WinitWindowAccessor;
        w.window().with_winit_window(|ww| apply_native_chrome(ww, dark));
        w.set_launch_on_startup(s.launch_on_startup);
        w.set_start_minimized(s.start_minimized);
        w.set_show_toast(s.show_toast);
    }

    fn push_shortcuts_to(&self, w: &MainWindow) {
        let cfg = self.cfg.borrow();
        w.set_mute_all_shortcut(label(cfg.mute_all.hotkey.as_deref()).into());
        w.set_restore_shortcut(label(cfg.mute_all.restore_hotkey.as_deref()).into());
    }

    fn set_status(&self, msg: &str, is_error: bool) {
        *self.status.borrow_mut() = (msg.to_owned(), is_error);
        if let Some(w) = self.ui() {
            w.set_status(msg.into());
            w.set_status_is_error(is_error);
        }
    }

    // ── Rows ───────────────────────────────────────────────────────────────────

    fn sync_rows(&self) {
        let cfg = self.cfg.borrow();
        let apps = self.apps.borrow();
        let icons = self.icons.borrow();
        let caps = self.caps.get();

        let mut rows: Vec<AppRow> = Vec::with_capacity(apps.len() + cfg.bindings.len());
        for a in apps.iter() {
            let status = if a.muted {
                "Muted".to_owned()
            } else if let (true, Some(v)) = (caps.per_app_volume, a.volume) {
                format!("Volume {}%", (v * 100.0).round())
            } else {
                "Active".to_owned()
            };
            rows.push(make_row(
                &a.key,
                &a.name,
                &format!("{status} · {}", a.key),
                true,
                a.muted,
                a.volume,
                &cfg,
                &icons,
            ));
        }
        // Bound apps that aren't running stay visible so their shortcut can be edited.
        for b in &cfg.bindings {
            if !apps.iter().any(|a| a.key == b.app) {
                let name = if b.display_name.is_empty() { &b.app } else { &b.display_name };
                rows.push(make_row(
                    &b.app,
                    name,
                    &format!("Not running · {}", b.app),
                    false,
                    false,
                    None,
                    &cfg,
                    &icons,
                ));
            }
        }

        let same_shape = self.rows.row_count() == rows.len()
            && rows.iter().enumerate().all(|(i, r)| self.rows.row_data(i).is_some_and(|old| old.key == r.key));
        if same_shape {
            for (i, r) in rows.into_iter().enumerate() {
                if self.rows.row_data(i).as_ref() != Some(&r) {
                    self.rows.set_row_data(i, r);
                }
            }
        } else {
            self.rows.set_vec(rows);
        }
        if let Some(w) = self.ui() {
            w.set_running_count(apps.len() as i32);
        }
    }

    // ── Audio events ───────────────────────────────────────────────────────────

    fn on_audio_event(&self, ev: AudioEvent) {
        match ev {
            AudioEvent::Ready { backend, caps, note } => {
                *self.backend.borrow_mut() = backend.to_owned();
                *self.platform_note.borrow_mut() = note.unwrap_or_default().to_owned();
                self.caps.set(caps);
                if let Some(w) = self.ui() {
                    self.push_all(&w);
                }
            }
            AudioEvent::Apps(apps) => {
                *self.apps.borrow_mut() = apps;
                self.sync_rows();
            }
            AudioEvent::Icon { key, icon } => {
                self.icons.borrow_mut().insert(key, crate::ui::image_from_icon(&icon));
                self.sync_rows();
            }
            AudioEvent::Toggled { outcome, from_hotkey } => {
                // Keep the friendly name for bindings of apps that later quit.
                let renamed = {
                    let mut cfg = self.cfg.borrow_mut();
                    match cfg.bindings.iter_mut().find(|b| b.app == outcome.key && b.display_name != outcome.name) {
                        Some(b) => {
                            b.display_name = outcome.name.clone();
                            true
                        }
                        None => false,
                    }
                };
                if renamed {
                    self.save();
                }
                if from_hotkey {
                    let detail = self.shortcut_label_for(&outcome.key);
                    let headline = format!("{} {}", outcome.name, if outcome.muted { "muted" } else { "unmuted" });
                    self.feedback(&headline, &detail, outcome.muted, false);
                }
            }
            AudioEvent::AllMuted { muted, from_hotkey } => {
                self.all_muted.set(muted);
                if let Some(t) = self.tray.borrow_mut().as_mut() {
                    t.set_all_muted(muted);
                }
                if let Some(w) = self.ui() {
                    w.set_all_muted(muted);
                }
                if from_hotkey {
                    let (headline, detail) = if muted {
                        ("Everything muted", "Press the shortcut again to restore")
                    } else {
                        ("Audio restored", "Apps are back to how they were")
                    };
                    self.feedback(headline, detail, muted, false);
                }
            }
            AudioEvent::NotRunning { key, from_hotkey } => {
                let name = self.display_name(&key);
                if from_hotkey {
                    self.feedback(&name, "Isn't playing audio right now", false, true);
                } else {
                    self.set_status(&format!("{name} isn't playing audio right now."), false);
                }
            }
            AudioEvent::Error { message, from_hotkey } => {
                log::warn!("{message}");
                if from_hotkey {
                    self.feedback("Couldn't change audio", &message, false, true);
                }
                self.set_status(&message, true);
            }
            AudioEvent::Fatal(message) => {
                *self.backend.borrow_mut() = "unavailable".into();
                self.set_status(&format!("Audio control unavailable: {message}"), true);
            }
        }
    }

    fn feedback(&self, headline: &str, detail: &str, muted: bool, error: bool) {
        if let Some(t) = self.tray.borrow().as_ref() {
            if !error {
                t.flash(muted, || {
                    with_app(|a| {
                        if let Some(t) = a.tray.borrow().as_ref() {
                            t.settle();
                        }
                    });
                });
            }
        }
        let (show, dark) = {
            let s = &self.cfg.borrow().settings;
            (s.show_toast, s.theme == Theme::Dark)
        };
        if show {
            self.toast.borrow_mut().show(dark, headline, detail, muted, error);
        }
    }

    fn display_name(&self, key: &str) -> String {
        if let Some(a) = self.apps.borrow().iter().find(|a| a.key == key) {
            return a.name.clone();
        }
        self.cfg
            .borrow()
            .binding_for(key)
            .map(|b| if b.display_name.is_empty() { b.app.clone() } else { b.display_name.clone() })
            .unwrap_or_else(|| key.to_owned())
    }

    fn shortcut_label_for(&self, key: &str) -> String {
        label(self.cfg.borrow().binding_for(key).map(|b| b.hotkey.as_str()))
    }

    // ── Shortcut capture ───────────────────────────────────────────────────────

    fn start_capture(&self, target: &str) {
        *self.capturing.borrow_mut() = Some(target.to_owned());
        // Release all global hotkeys so the combo reaches our window instead of firing.
        if let Some(h) = self.hotkeys.borrow_mut().as_mut() {
            h.suspend();
        }
        if let Some(w) = self.ui() {
            w.set_capturing(target.into());
        }
    }

    fn end_capture(&self) {
        *self.capturing.borrow_mut() = None;
        if let Some(w) = self.ui() {
            w.set_capturing(SharedString::default());
        }
        self.apply_hotkeys();
    }

    fn key_captured(&self, text: &str, ctrl: bool, alt: bool, shift: bool, meta: bool) {
        let Some(target) = self.capturing.borrow().clone() else { return };
        match capture_from_key_event(text, ctrl, alt, shift, meta) {
            Capture::Pending => {}
            Capture::Cancel => self.end_capture(),
            Capture::Clear => self.set_shortcut(&target, None),
            Capture::Invalid(why) => self.set_status(why, true),
            Capture::Captured(acc) => {
                if let Some(owner) = self.conflict(&target, acc) {
                    self.set_status(&format!("{} is already used by {owner}.", acc.label()), true);
                    return;
                }
                self.set_shortcut(&target, Some(acc));
            }
        }
    }

    /// Who else already uses `acc`, if anyone (ignoring `target` itself).
    fn conflict(&self, target: &str, acc: Accelerator) -> Option<String> {
        let cfg = self.cfg.borrow();
        let same = |h: &str| h.parse::<Accelerator>().ok() == Some(acc);
        if target != TARGET_ALL && cfg.mute_all.hotkey.as_deref().is_some_and(same) {
            return Some("Mute All".into());
        }
        if target != TARGET_RESTORE && cfg.mute_all.restore_hotkey.as_deref().is_some_and(same) {
            return Some("Restore All".into());
        }
        cfg.bindings.iter().find(|b| b.app != target && same(&b.hotkey)).map(|b| {
            if b.display_name.is_empty() {
                b.app.clone()
            } else {
                b.display_name.clone()
            }
        })
    }

    fn set_shortcut(&self, target: &str, acc: Option<Accelerator>) {
        let value = acc.map(|a| a.to_string());
        {
            let mut cfg = self.cfg.borrow_mut();
            match target {
                TARGET_ALL => cfg.mute_all.hotkey = value,
                TARGET_RESTORE => cfg.mute_all.restore_hotkey = value,
                key => {
                    let name = self.display_name(key);
                    cfg.set_binding(key, &name, value);
                }
            }
        }
        self.save();
        self.set_status("", false);
        if let Some(w) = self.ui() {
            self.push_shortcuts_to(&w);
        }
        self.sync_rows();
        if self.capturing.borrow().is_some() {
            self.end_capture(); // re-applies hotkeys
        } else {
            self.apply_hotkeys();
        }
    }

    fn apply_hotkeys(&self) {
        let errors = match self.hotkeys.borrow_mut().as_mut() {
            Some(h) => h.apply(&self.cfg.borrow()),
            None => return,
        };
        if !errors.is_empty() {
            self.set_status(&errors.join("\n"), true);
        }
    }

    // ── Settings ───────────────────────────────────────────────────────────────

    fn update_settings(&self, f: impl FnOnce(&mut crate::config::Settings)) {
        f(&mut self.cfg.borrow_mut().settings);
        self.save();
        self.push_settings();
    }

    fn save(&self) {
        if let Err(e) = self.store.save(&self.cfg.borrow()) {
            log::error!("saving config: {e}");
            self.set_status(&format!("Couldn't save settings: {e}"), true);
        }
    }
}

/// Title-bar theme (dark title bar on Windows 10/11, appearance on macOS) and window icon.
fn apply_native_chrome(w: &slint::winit_030::winit::window::Window, dark: bool) {
    use slint::winit_030::winit::window::{Icon, Theme as WinitTheme};
    w.set_theme(Some(if dark { WinitTheme::Dark } else { WinitTheme::Light }));
    // Small icon: title bar / Alt+Tab.
    if let Ok(icon) = Icon::from_rgba(icon::render(32, IconState::Normal), 32, 32) {
        w.set_window_icon(Some(icon));
    }
    // Windows uses a separate "big" icon for the taskbar; winit only sets it on request.
    #[cfg(target_os = "windows")]
    {
        use slint::winit_030::winit::platform::windows::WindowExtWindows;
        if let Ok(icon) = Icon::from_rgba(icon::render(64, IconState::Normal), 64, 64) {
            w.set_taskbar_icon(Some(icon));
        }
    }
}

fn label(hotkey: Option<&str>) -> String {
    hotkey.and_then(|h| h.parse::<Accelerator>().ok()).map(|a| a.label()).unwrap_or_default()
}

#[allow(clippy::too_many_arguments)]
fn make_row(
    key: &str,
    name: &str,
    subtitle: &str,
    running: bool,
    muted: bool,
    volume: Option<f32>,
    cfg: &Config,
    icons: &HashMap<String, slint::Image>,
) -> AppRow {
    let icon = icons.get(key).cloned();
    AppRow {
        key: key.into(),
        name: name.into(),
        subtitle: subtitle.into(),
        has_icon: icon.is_some(),
        icon: icon.unwrap_or_default(),
        initial: crate::ui::initial_for(name).into(),
        tint: crate::ui::tint_for(key),
        running,
        muted,
        volume: volume.unwrap_or(-1.0),
        shortcut: label(cfg.binding_for(key).map(|b| b.hotkey.as_str())).into(),
    }
}
