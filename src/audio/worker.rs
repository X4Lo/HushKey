//! Dedicated audio thread.
//!
//! The backend lives entirely on this thread; everyone else talks to it through
//! [`AudioHandle`] (commands in) and an event sink (events out). Hotkey presses go
//! straight here from the OS hotkey callback, so muting never waits on the UI.
//!
//! The thread blocks on its channel when idle — zero CPU. The only periodic work is
//! while "Mute All" is engaged: every 2 s it silences apps that started afterwards.

use super::mute::ToggleOutcome;
use super::{create_controller, AudioApp, AudioError, Capabilities, IconData, MuteManager};
use std::collections::HashSet;
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
    Shutdown,
}

#[derive(Debug, Clone)]
pub enum AudioEvent {
    Ready {
        backend: &'static str,
        caps: Capabilities,
        note: Option<&'static str>,
    },
    Apps(Vec<AudioApp>),
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

            let publish_apps =
                |ctl: &mut dyn super::AudioController, icons_sent: &mut HashSet<String>| match ctl.list_audio_apps() {
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
                };

            loop {
                let cmd = if mute.all_muted() {
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
