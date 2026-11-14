//! In-memory backend: used by unit tests and as the fallback on unsupported OSes.

use super::{AudioApp, AudioController, AudioError, AudioResult, Capabilities};

#[derive(Default)]
pub struct NullController {
    pub apps: Vec<AudioApp>,
    /// Count of set_app_mute calls, for tests.
    pub mute_calls: usize,
}

impl NullController {
    #[cfg(test)]
    pub fn with_apps(keys: &[(&str, bool)]) -> Self {
        Self {
            apps: keys
                .iter()
                .map(|(k, muted)| AudioApp {
                    key: (*k).into(),
                    name: (*k).into(),
                    pids: vec![1],
                    volume: Some(1.0),
                    muted: *muted,
                    exe_path: None,
                })
                .collect(),
            mute_calls: 0,
        }
    }

    pub fn is_muted(&self, key: &str) -> Option<bool> {
        self.apps.iter().find(|a| a.key == key).map(|a| a.muted)
    }
}

impl AudioController for NullController {
    fn backend_name(&self) -> &'static str {
        "none"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities { per_app_volume: true, per_app_mute: true }
    }

    fn list_audio_apps(&mut self) -> AudioResult<Vec<AudioApp>> {
        Ok(self.apps.clone())
    }

    fn set_app_volume(&mut self, app: &str, volume: f32) -> AudioResult<()> {
        let a = self.apps.iter_mut().find(|a| a.key == app).ok_or_else(|| AudioError::NotRunning(app.into()))?;
        a.volume = Some(volume.clamp(0.0, 1.0));
        Ok(())
    }

    fn set_app_mute(&mut self, app: &str, muted: bool) -> AudioResult<()> {
        self.mute_calls += 1;
        let a = self.apps.iter_mut().find(|a| a.key == app).ok_or_else(|| AudioError::NotRunning(app.into()))?;
        a.muted = muted;
        Ok(())
    }

    fn platform_note(&self) -> Option<&'static str> {
        Some("Per-app audio control isn't available on this platform.")
    }
}
