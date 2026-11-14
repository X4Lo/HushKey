//! System-wide hotkey registration.
//!
//! Uses the `global-hotkey` crate:
//! - Windows: `RegisterHotKey` — delivered even while a fullscreen game has focus.
//! - macOS:   Carbon `RegisterEventHotKey` — no Accessibility permission required.
//! - Linux:   X11 `XGrabKey`. **Wayland has no global-hotkey protocol**; under Wayland
//!   shortcuts only work for XWayland clients or via the desktop's own shortcut
//!   settings (see docs/design-decisions.md).
//!
//! The manager must be created and used on the main (event-loop) thread. The press
//! handler, however, runs wherever the OS delivers the event and forwards the action
//! straight to the audio worker — no UI round-trip on the hot path.

mod accelerator;

pub use accelerator::{capture_from_key_event, Accelerator, Capture};

use crate::config::Config;
use global_hotkey::{GlobalHotKeyEvent, GlobalHotKeyManager, HotKeyState};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HotkeyAction {
    ToggleApp(String),
    ToggleAll,
    RestoreAll,
}

type ActionMap = Arc<Mutex<HashMap<u32, HotkeyAction>>>;

pub struct Hotkeys {
    manager: GlobalHotKeyManager,
    registered: Vec<global_hotkey::hotkey::HotKey>,
    actions: ActionMap,
}

impl Hotkeys {
    /// Create the manager and install `on_action`, called on every key *press*.
    pub fn new(on_action: impl Fn(HotkeyAction) + Send + Sync + 'static) -> Result<Self, String> {
        let manager = GlobalHotKeyManager::new().map_err(|e| e.to_string())?;
        let actions: ActionMap = Arc::default();
        let lookup = actions.clone();
        GlobalHotKeyEvent::set_event_handler(Some(move |ev: GlobalHotKeyEvent| {
            if ev.state() != HotKeyState::Pressed {
                return;
            }
            let action = lookup.lock().ok().and_then(|m| m.get(&ev.id()).cloned());
            if let Some(action) = action {
                on_action(action);
            }
        }));
        Ok(Self { manager, registered: Vec::new(), actions })
    }

    /// Unregister everything (e.g. while the user is recording a new shortcut, so the
    /// key combo reaches our window instead of firing the old binding).
    pub fn suspend(&mut self) {
        if !self.registered.is_empty() {
            let _ = self.manager.unregister_all(&self.registered);
            self.registered.clear();
        }
        if let Ok(mut m) = self.actions.lock() {
            m.clear();
        }
    }

    /// Re-register every shortcut in `cfg`. Returns human-readable failures
    /// (typically "already registered by another program").
    pub fn apply(&mut self, cfg: &Config) -> Vec<String> {
        self.suspend();
        let mut wanted: Vec<(String, HotkeyAction, String)> = Vec::new();
        if let Some(h) = &cfg.mute_all.hotkey {
            wanted.push((h.clone(), HotkeyAction::ToggleAll, "Mute All".into()));
        }
        if let Some(h) = &cfg.mute_all.restore_hotkey {
            wanted.push((h.clone(), HotkeyAction::RestoreAll, "Restore All".into()));
        }
        for b in &cfg.bindings {
            let label = if b.display_name.is_empty() { b.app.clone() } else { b.display_name.clone() };
            wanted.push((b.hotkey.clone(), HotkeyAction::ToggleApp(b.app.clone()), label));
        }

        let mut errors = Vec::new();
        let mut map = HashMap::new();
        for (text, action, label) in wanted {
            let acc = match text.parse::<Accelerator>() {
                Ok(a) => a,
                Err(e) => {
                    errors.push(format!("{label}: {e}"));
                    continue;
                }
            };
            let hk = acc.to_hotkey();
            if map.contains_key(&hk.id()) {
                errors.push(format!("{label}: {} is already used by another binding", acc.label()));
                continue;
            }
            match self.manager.register(hk) {
                Ok(()) => {
                    self.registered.push(hk);
                    map.insert(hk.id(), action);
                }
                Err(e) => {
                    log::warn!("register {text} failed: {e}");
                    errors.push(format!("{label}: {} is taken by another program", acc.label()));
                }
            }
        }
        if let Ok(mut m) = self.actions.lock() {
            *m = map;
        }
        errors
    }
}

impl Drop for Hotkeys {
    fn drop(&mut self) {
        self.suspend();
        GlobalHotKeyEvent::set_event_handler(None::<fn(GlobalHotKeyEvent)>);
    }
}
