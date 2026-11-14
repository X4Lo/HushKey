//! System tray / menu-bar icon.
//!
//! - Windows/macOS: native tray, created on the main thread after the event loop starts.
//! - Linux: StatusNotifierItem over D-Bus (`ksni` feature) — works on KDE, GNOME with
//!   the AppIndicator extension, XFCE, Cinnamon, etc. No GTK main loop required.
//!
//! The icon reflects the Mute All state persistently and "flashes" to the muted/unmuted
//! glyph for a moment whenever a shortcut fires, as glanceable feedback.

use super::icon::{self, IconState};
use std::time::Duration;
use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};

pub const MENU_OPEN: &str = "open";
pub const MENU_TOGGLE_ALL: &str = "toggle-all";
pub const MENU_QUIT: &str = "quit";

const ICON_PX: u32 = 32;
const FLASH: Duration = Duration::from_millis(900);

#[derive(Debug, Clone, Copy)]
pub enum TrayCommand {
    Open,
    ToggleAll,
    Quit,
}

pub struct Tray {
    tray: TrayIcon,
    toggle_item: MenuItem,
    all_muted: bool,
    flash_timer: slint::Timer,
}

fn icon(state: IconState) -> Option<Icon> {
    Icon::from_rgba(icon::render(ICON_PX, state), ICON_PX, ICON_PX).ok()
}

impl Tray {
    /// `on_command` is invoked on the Slint event loop thread.
    pub fn new(on_command: impl Fn(TrayCommand) + Send + Sync + Clone + 'static) -> Result<Self, String> {
        let menu = Menu::new();
        let open = MenuItem::with_id(MENU_OPEN, "Open HushKey", true, None);
        let toggle_item = MenuItem::with_id(MENU_TOGGLE_ALL, "Mute all", true, None);
        let quit = MenuItem::with_id(MENU_QUIT, "Quit HushKey", true, None);
        menu.append_items(&[&open, &toggle_item, &PredefinedMenuItem::separator(), &quit])
            .map_err(|e| e.to_string())?;

        let tray = TrayIconBuilder::new()
            .with_menu(Box::new(menu))
            .with_menu_on_left_click(false)
            .with_tooltip("HushKey")
            .with_icon(icon(IconState::Normal).ok_or("icon")?)
            .build()
            .map_err(|e| e.to_string())?;

        let cmd = on_command.clone();
        MenuEvent::set_event_handler(Some(move |e: MenuEvent| {
            let c = match e.id.0.as_str() {
                MENU_OPEN => TrayCommand::Open,
                MENU_TOGGLE_ALL => TrayCommand::ToggleAll,
                MENU_QUIT => TrayCommand::Quit,
                _ => return,
            };
            let cmd = cmd.clone();
            let _ = slint::invoke_from_event_loop(move || cmd(c));
        }));
        TrayIconEvent::set_event_handler(Some(move |e: TrayIconEvent| {
            let open = matches!(
                e,
                TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. }
                    | TrayIconEvent::DoubleClick { button: MouseButton::Left, .. }
            );
            if open {
                let cmd = on_command.clone();
                let _ = slint::invoke_from_event_loop(move || cmd(TrayCommand::Open));
            }
        }));

        Ok(Self { tray, toggle_item, all_muted: false, flash_timer: slint::Timer::default() })
    }

    fn resting_state(&self) -> IconState {
        if self.all_muted {
            IconState::Muted
        } else {
            IconState::Normal
        }
    }

    pub fn set_all_muted(&mut self, muted: bool) {
        self.all_muted = muted;
        self.toggle_item.set_text(if muted { "Restore all" } else { "Mute all" });
        let _ = self.tray.set_tooltip(Some(if muted { "HushKey — everything muted" } else { "HushKey" }));
        if !self.flash_timer.running() {
            let _ = self.tray.set_icon(icon(self.resting_state()));
        }
    }

    /// Briefly show the muted/unmuted glyph, then return to the resting state.
    pub fn flash(&self, muted: bool, restore: impl FnMut() + 'static) {
        let _ = self.tray.set_icon(icon(if muted { IconState::Muted } else { IconState::Normal }));
        self.flash_timer.start(slint::TimerMode::SingleShot, FLASH, restore);
    }

    pub fn settle(&self) {
        let _ = self.tray.set_icon(icon(self.resting_state()));
    }
}

impl Drop for Tray {
    fn drop(&mut self) {
        MenuEvent::set_event_handler(None::<fn(MenuEvent)>);
        TrayIconEvent::set_event_handler(None::<fn(TrayIconEvent)>);
    }
}
