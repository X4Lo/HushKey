//! Toggle / mute-all state machine, independent of any OS backend.

use super::{AudioController, AudioError, AudioResult, MuteSnapshot};

/// Result of a toggle, for feedback (toast / tray flash).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToggleOutcome {
    pub key: String,
    pub name: String,
    pub muted: bool,
}

#[derive(Default)]
pub struct MuteManager {
    /// Present while "Mute All" is engaged.
    all: Option<MuteSnapshot>,
}

impl MuteManager {
    pub fn all_muted(&self) -> bool {
        self.all.is_some()
    }

    /// Toggle one app: if any of its sessions is audible, mute them all; otherwise unmute.
    pub fn toggle_app(&mut self, ctl: &mut dyn AudioController, key: &str) -> AudioResult<ToggleOutcome> {
        let app = ctl
            .list_audio_apps()?
            .into_iter()
            .find(|a| a.key == key)
            .ok_or_else(|| AudioError::NotRunning(key.into()))?;
        let mute = !app.muted;
        ctl.set_app_mute(key, mute)?;
        // If the user manually unmutes an app during Mute All, Restore All must not touch it
        // again; if they manually mute it, it was their choice, so restore leaves it muted.
        if let Some(snap) = &mut self.all {
            snap.muted_keys.retain(|k| k != key);
        }
        Ok(ToggleOutcome { key: app.key, name: app.name, muted: mute })
    }

    pub fn mute_all(&mut self, ctl: &mut dyn AudioController) -> AudioResult<()> {
        let snap = ctl.mute_all()?;
        match &mut self.all {
            // Already engaged (e.g. re-applied after new apps appeared): extend the snapshot.
            Some(existing) => {
                for k in snap.muted_keys {
                    if !existing.muted_keys.contains(&k) {
                        existing.muted_keys.push(k);
                    }
                }
            }
            None => self.all = Some(snap),
        }
        Ok(())
    }

    pub fn restore_all(&mut self, ctl: &mut dyn AudioController) -> AudioResult<()> {
        if let Some(snap) = self.all.take() {
            ctl.restore_all(&snap)?;
        }
        Ok(())
    }

    /// Returns the new "all muted" state.
    pub fn toggle_all(&mut self, ctl: &mut dyn AudioController) -> AudioResult<bool> {
        if self.all_muted() {
            self.restore_all(ctl)?;
        } else {
            self.mute_all(ctl)?;
        }
        Ok(self.all_muted())
    }

    /// While Mute All is engaged, silence apps that started playing afterwards.
    pub fn enforce(&mut self, ctl: &mut dyn AudioController) -> AudioResult<()> {
        if self.all_muted() {
            self.mute_all(ctl)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::null::NullController;

    #[test]
    fn toggle_flips_state() {
        let mut ctl = NullController::with_apps(&[("a", false)]);
        let mut m = MuteManager::default();
        assert!(m.toggle_app(&mut ctl, "a").unwrap().muted);
        assert_eq!(ctl.is_muted("a"), Some(true));
        assert!(!m.toggle_app(&mut ctl, "a").unwrap().muted);
        assert_eq!(ctl.is_muted("a"), Some(false));
    }

    #[test]
    fn toggle_missing_app_is_not_running() {
        let mut ctl = NullController::with_apps(&[]);
        let err = MuteManager::default().toggle_app(&mut ctl, "ghost").unwrap_err();
        assert_eq!(err, AudioError::NotRunning("ghost".into()));
    }

    #[test]
    fn restore_all_leaves_previously_muted_apps_muted() {
        let mut ctl = NullController::with_apps(&[("a", false), ("b", true)]);
        let mut m = MuteManager::default();
        assert!(m.toggle_all(&mut ctl).unwrap());
        assert_eq!(ctl.is_muted("a"), Some(true));
        assert!(!m.toggle_all(&mut ctl).unwrap());
        assert_eq!(ctl.is_muted("a"), Some(false));
        assert_eq!(ctl.is_muted("b"), Some(true), "user had muted b before; keep it muted");
    }

    #[test]
    fn manual_toggle_during_mute_all_is_respected() {
        let mut ctl = NullController::with_apps(&[("a", false), ("b", false)]);
        let mut m = MuteManager::default();
        m.mute_all(&mut ctl).unwrap();
        m.toggle_app(&mut ctl, "a").unwrap(); // user unmutes a
        m.toggle_app(&mut ctl, "a").unwrap(); // ...and mutes it again manually
        m.restore_all(&mut ctl).unwrap();
        assert_eq!(ctl.is_muted("a"), Some(true));
        assert_eq!(ctl.is_muted("b"), Some(false));
    }

    #[test]
    fn enforce_mutes_late_starters_and_restores_them() {
        let mut ctl = NullController::with_apps(&[("a", false)]);
        let mut m = MuteManager::default();
        m.mute_all(&mut ctl).unwrap();
        ctl.apps.extend(NullController::with_apps(&[("late", false)]).apps);
        m.enforce(&mut ctl).unwrap();
        assert_eq!(ctl.is_muted("late"), Some(true));
        m.restore_all(&mut ctl).unwrap();
        assert_eq!(ctl.is_muted("late"), Some(false));
        assert_eq!(ctl.is_muted("a"), Some(false));
    }

    #[test]
    fn enforce_is_noop_when_not_engaged() {
        let mut ctl = NullController::with_apps(&[("a", false)]);
        MuteManager::default().enforce(&mut ctl).unwrap();
        assert_eq!(ctl.mute_calls, 0);
    }
}
