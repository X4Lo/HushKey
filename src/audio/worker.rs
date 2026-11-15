//! Dedicated audio thread.
//!
//! The backend lives entirely on this thread; everyone else talks to it through
//! [`AudioHandle`] (commands in) and an event sink (events out). Hotkey presses go
//! straight here from the OS hotkey callback, so muting never waits on the UI.
//!
//! The thread blocks on its channel when idle — zero CPU. The only periodic work is
//! while "Mute All" is engaged: every 2 s it silences apps that started afterwards.
//!
//! Volume sliders send a command per mouse move; consecutive volume commands for the
//! same target are coalesced so only the latest level is applied.

use super::mute::ToggleOutcome;
use super::{
    create_controller, AudioApp, AudioController, AudioError, Capabilities, IconData, MasterVolume, MuteManager,
    OutputDevice,
};
use std::collections::{HashSet, VecDeque};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::thread::JoinHandle;
use std::time::Duration;

const ENFORCE_INTERVAL: Duration = Duration::from_secs(2);

#[derive(Debug, Clone)]
pub enum AudioCommand {
    Refresh,
    /// `from_hotkey` controls whether the user gets toast/tray feedback.
    ToggleApp {
        key: String,
        from_hotkey: bool,
    },
    ToggleAll {
        from_hotkey: bool,
    },
    RestoreAll {
        from_hotkey: bool,
    },
    SetAppVolume {
        key: String,
        volume: f32,
    },
    SetMasterVolume(f32),
    SetMasterMute(bool),
    SetDefaultDevice(String),
    /// `device: None` routes the app back to the system default.
    SetAppDevice {
        key: String,
        device: Option<String>,
    },
    Shutdown,
}

impl AudioCommand {
    /// Commands where only the most recent one matters, keyed by target.
    fn coalesce_key(&self) -> Option<&str> {
        match self {
            AudioCommand::SetAppVolume { key, .. } => Some(key),
            AudioCommand::SetMasterVolume(_) => Some("@master"),
            _ => None,
        }
    }
}

/// Replace `cmd` with the newest queued command for the same target, removing the stale ones.
fn coalesce(mut cmd: AudioCommand, pending: &mut VecDeque<AudioCommand>) -> AudioCommand {
    let Some(target) = cmd.coalesce_key().map(str::to_owned) else { return cmd };
    while let Some(pos) = pending.iter().position(|c| c.coalesce_key() == Some(&target)) {
        if let Some(newer) = pending.remove(pos) {
            cmd = newer;
        }
    }
    cmd
}

#[derive(Debug, Clone)]
pub enum AudioEvent {
    Ready {
        backend: &'static str,
        caps: Capabilities,
        note: Option<&'static str>,
    },
    Apps(Vec<AudioApp>),
    /// Output devices and system volume. `master` is `None` when unsupported/unavailable.
    Output {
        devices: Vec<OutputDevice>,
        master: Option<MasterVolume>,
    },
    Icon {
        key: String,
        icon: IconData,
    },
    Toggled {
        outcome: ToggleOutcome,
        from_hotkey: bool,
    },
    AllMuted {
        muted: bool,
        from_hotkey: bool,
    },
    /// A per-app shortcut fired but that app has no audio session right now.
    NotRunning {
        key: String,
        from_hotkey: bool,
    },
    Error {
        message: String,
        from_hotkey: bool,
    },
    /// The backend couldn't start at all (e.g. no PulseAudio server).
    Fatal(String),
}

pub struct AudioHandle {
    tx: Sender<AudioCommand>,
    thread: Option<JoinHandle<()>>,
}

impl AudioHandle {
    pub fn sender(&self) -> Sender<AudioCommand> {
        self.tx.clone()
    }

    pub fn send(&self, cmd: AudioCommand) {
        let _ = self.tx.send(cmd);
    }

    /// Restores anything Mute All silenced, then joins the thread.
    pub fn shutdown(mut self) {
        let _ = self.tx.send(AudioCommand::Shutdown);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

pub fn spawn(sink: impl Fn(AudioEvent) + Send + 'static) -> AudioHandle {
    let (tx, rx) = mpsc::channel::<AudioCommand>();
    let thread = std::thread::Builder::new()
        .name("hushkey-audio".into())
        .spawn(move || {
            let mut ctl = match create_controller() {
                Ok(c) => c,
                Err(e) => {
                    log::error!("audio backend failed to start: {e}");
                    sink(AudioEvent::Fatal(e.to_string()));
                    // Keep draining so senders never block/err; nothing to control.
                    while let Ok(cmd) = rx.recv() {
                        if matches!(cmd, AudioCommand::Shutdown) {
                            break;
                        }
                    }
                    return;
                }
            };
            log::info!("audio backend: {}", ctl.backend_name());
            sink(AudioEvent::Ready {
                backend: ctl.backend_name(),
                caps: ctl.capabilities(),
                note: ctl.platform_note(),
            });

            let mut mute = MuteManager::default();
            let mut icons_sent: HashSet<String> = HashSet::new();

            let caps = ctl.capabilities();
            let publish_apps = |ctl: &mut dyn AudioController, icons_sent: &mut HashSet<String>| {
                match ctl.list_audio_apps() {
                    Ok(apps) => {
                        for app in &apps {
                            if icons_sent.insert(app.key.clone()) {
                                if let Some(icon) = ctl.app_icon(app) {
                                    sink(AudioEvent::Icon { key: app.key.clone(), icon });
                                }
                            }
                        }
                        sink(AudioEvent::Apps(apps));
                    }
                    Err(e) => sink(AudioEvent::Error { message: e.to_string(), from_hotkey: false }),
                }
                if caps.output_devices || caps.master_volume {
                    let devices = ctl.output_devices().unwrap_or_else(|e| {
                        log::debug!("output devices: {e}");
                        Vec::new()
                    });
                    let master = if caps.master_volume { ctl.master_volume().ok() } else { None };
                    sink(AudioEvent::Output { devices, master });
                }
            };
            let report = |result: Result<(), AudioError>, what: &str| {
                if let Err(e) = result {
                    sink(AudioEvent::Error { message: format!("{what}: {e}"), from_hotkey: false });
                }
            };
            let mut pending: VecDeque<AudioCommand> = VecDeque::new();

            loop {
                let cmd = if let Some(cmd) = pending.pop_front() {
                    cmd
                } else if mute.all_muted() {
                    match rx.recv_timeout(ENFORCE_INTERVAL) {
                        Ok(cmd) => cmd,
                        Err(RecvTimeoutError::Timeout) => {
                            if let Err(e) = mute.enforce(ctl.as_mut()) {
                                log::debug!("enforce: {e}");
                            }
                            continue;
                        }
                        Err(RecvTimeoutError::Disconnected) => AudioCommand::Shutdown,
                    }
                } else {
                    rx.recv().unwrap_or(AudioCommand::Shutdown)
                };
                let cmd = if cmd.coalesce_key().is_some() {
                    pending.extend(rx.try_iter());
                    coalesce(cmd, &mut pending)
                } else {
                    cmd
                };

                match cmd {
                    AudioCommand::Refresh => {
                        let _ = mute.enforce(ctl.as_mut());
                        publish_apps(ctl.as_mut(), &mut icons_sent);
                    }
                    AudioCommand::ToggleApp { key, from_hotkey } => {
                        match mute.toggle_app(ctl.as_mut(), &key) {
                            Ok(outcome) => sink(AudioEvent::Toggled { outcome, from_hotkey }),
                            Err(AudioError::NotRunning(_)) => sink(AudioEvent::NotRunning { key, from_hotkey }),
                            Err(e) => sink(AudioEvent::Error { message: format!("{key}: {e}"), from_hotkey }),
                        }
                        publish_apps(ctl.as_mut(), &mut icons_sent);
                    }
                    AudioCommand::ToggleAll { from_hotkey } => {
                        match mute.toggle_all(ctl.as_mut()) {
                            Ok(muted) => sink(AudioEvent::AllMuted { muted, from_hotkey }),
                            Err(e) => sink(AudioEvent::Error { message: e.to_string(), from_hotkey }),
                        }
                        publish_apps(ctl.as_mut(), &mut icons_sent);
                    }
                    AudioCommand::RestoreAll { from_hotkey } => {
                        match mute.restore_all(ctl.as_mut()) {
                            Ok(()) => sink(AudioEvent::AllMuted { muted: false, from_hotkey }),
                            Err(e) => sink(AudioEvent::Error { message: e.to_string(), from_hotkey }),
                        }
                        publish_apps(ctl.as_mut(), &mut icons_sent);
                    }
                    // Sliders: the UI already shows the new level, so no full republish per move.
                    AudioCommand::SetAppVolume { key, volume } => match ctl.set_app_volume(&key, volume) {
                        Ok(()) | Err(AudioError::NotRunning(_)) => {}
                        Err(e) => report(Err(e), &key),
                    },
                    AudioCommand::SetMasterVolume(v) => report(ctl.set_master_volume(v), "System volume"),
                    AudioCommand::SetMasterMute(m) => {
                        report(ctl.set_master_mute(m), "System mute");
                        publish_apps(ctl.as_mut(), &mut icons_sent);
                    }
                    AudioCommand::SetDefaultDevice(id) => {
                        report(ctl.set_default_output_device(&id), "Output device");
                        publish_apps(ctl.as_mut(), &mut icons_sent);
                    }
                    AudioCommand::SetAppDevice { key, device } => {
                        report(ctl.set_app_output_device(&key, device.as_deref()), &key);
                        publish_apps(ctl.as_mut(), &mut icons_sent);
                    }
                    AudioCommand::Shutdown => {
                        if let Err(e) = mute.restore_all(ctl.as_mut()) {
                            log::warn!("restore on shutdown failed: {e}");
                        }
                        break;
                    }
                }
            }
        })
        .expect("spawn audio thread");
    AudioHandle { tx, thread: Some(thread) }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vol(key: &str, v: f32) -> AudioCommand {
        AudioCommand::SetAppVolume { key: key.into(), volume: v }
    }

    #[test]
    fn coalesces_same_target_and_keeps_others_in_order() {
        let mut pending: VecDeque<AudioCommand> =
            [vol("a", 0.2), AudioCommand::Refresh, vol("b", 0.9), vol("a", 0.7), AudioCommand::SetMasterVolume(0.4)]
                .into_iter()
                .collect();
        let cmd = coalesce(vol("a", 0.1), &mut pending);
        assert!(matches!(cmd, AudioCommand::SetAppVolume { ref key, volume } if key == "a" && volume == 0.7));
        assert_eq!(pending.len(), 3);
        assert!(matches!(pending[0], AudioCommand::Refresh));
        assert!(matches!(pending[1], AudioCommand::SetAppVolume { ref key, .. } if key == "b"));
        assert!(matches!(pending[2], AudioCommand::SetMasterVolume(_)));
    }

    #[test]
    fn non_volume_commands_pass_through() {
        let mut pending: VecDeque<AudioCommand> = [AudioCommand::Refresh].into_iter().collect();
        assert!(matches!(coalesce(AudioCommand::SetMasterMute(true), &mut pending), AudioCommand::SetMasterMute(true)));
        assert_eq!(pending.len(), 1);
    }
}
