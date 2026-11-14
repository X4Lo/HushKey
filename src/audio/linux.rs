//! Linux backend: PulseAudio sink inputs (also works on PipeWire via `pipewire-pulse`,
//! the default on Fedora, Ubuntu 22.10+, Arch, etc.).
//!
//! Every playback stream an app opens is a *sink input*; that includes paused/corked
//! streams, so idle-but-registered apps show up too. Full per-app volume and mute are
//! natively supported.
//!
//! Uses the single-threaded standard mainloop driven synchronously from the audio worker
//! thread: each call issues an operation and iterates the loop until it completes.

use super::{AudioApp, AudioController, AudioError, AudioResult, Capabilities};
use libpulse_binding as pulse;
use pulse::callbacks::ListResult;
use pulse::context::{Context, FlagSet, State};
use pulse::mainloop::standard::{IterateResult, Mainloop};
use pulse::operation::{Operation, State as OpState};
use pulse::proplist::{properties, Proplist};
use pulse::volume::{ChannelVolumes, Volume};
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;

#[derive(Clone)]
struct Input {
    index: u32,
    key: String,
    name: String,
    pid: u32,
    muted: bool,
    volume: f32,
    channels: u8,
}

pub struct PulseController {
    mainloop: Mainloop,
    context: Context,
}

fn backend(msg: impl Into<String>) -> AudioError {
    AudioError::Backend(msg.into())
}

impl PulseController {
    pub fn new() -> AudioResult<Self> {
        let mut props = Proplist::new().ok_or_else(|| backend("Proplist::new failed"))?;
        let _ = props.set_str(properties::APPLICATION_NAME, "HushKey");
        let _ = props.set_str(properties::APPLICATION_ID, "io.github.hushkey");
        let mut mainloop = Mainloop::new().ok_or_else(|| backend("PulseAudio mainloop unavailable"))?;
        let mut context = Context::new_with_proplist(&mainloop, "HushKey", &props)
            .ok_or_else(|| backend("PulseAudio context failed"))?;
        context
            .connect(None, FlagSet::NOFLAGS, None)
            .map_err(|e| backend(format!("cannot connect to PulseAudio/PipeWire: {e}")))?;
        loop {
            match mainloop.iterate(true) {
                IterateResult::Success(_) => {}
                IterateResult::Quit(_) | IterateResult::Err(_) => return Err(backend("PulseAudio mainloop failed")),
            }
            match context.get_state() {
                State::Ready => break,
                State::Failed | State::Terminated => {
                    return Err(backend("no PulseAudio server (is pipewire-pulse or pulseaudio running?)"))
                }
                _ => {}
            }
        }
        Ok(Self { mainloop, context })
    }

    fn wait<T: ?Sized>(&mut self, op: Operation<T>) -> AudioResult<()> {
        while op.get_state() == OpState::Running {
            if let IterateResult::Quit(_) | IterateResult::Err(_) = self.mainloop.iterate(true) {
                return Err(backend("PulseAudio connection lost"));
            }
        }
        Ok(())
    }

    fn inputs(&mut self) -> AudioResult<Vec<Input>> {
        let out: Rc<RefCell<Vec<Input>>> = Rc::default();
        let sink = out.clone();
        let op = self.context.introspect().get_sink_input_info_list(move |r| {
            if let ListResult::Item(i) = r {
                let get = |k: &str| i.proplist.get_str(k).filter(|s| !s.is_empty());
                let binary = get(properties::APPLICATION_PROCESS_BINARY);
                let app_name = get(properties::APPLICATION_NAME);
                let key = binary.clone().or_else(|| app_name.clone()).unwrap_or_else(|| format!("stream-{}", i.index));
                let name = app_name.or(binary).unwrap_or_else(|| key.clone());
                let pid = get(properties::APPLICATION_PROCESS_ID).and_then(|p| p.parse().ok()).unwrap_or(0);
                sink.borrow_mut().push(Input {
                    index: i.index,
                    key: key.to_lowercase(),
                    name,
                    pid,
                    muted: i.mute,
                    volume: i.volume.avg().0 as f32 / Volume::NORMAL.0 as f32,
                    channels: i.volume.len(),
                });
            }
        });
        self.wait(op)?;
        let v = out.borrow().clone();
        Ok(v)
    }

    fn for_each_input_of(
        &mut self,
        app: &str,
        mut f: impl FnMut(&mut Self, &Input) -> Operation<dyn FnMut(bool)>,
    ) -> AudioResult<()> {
        let inputs: Vec<Input> = self.inputs()?.into_iter().filter(|i| i.key == app).collect();
        if inputs.is_empty() {
            return Err(AudioError::NotRunning(app.into()));
        }
        for input in &inputs {
            let op = f(self, input);
            self.wait(op)?;
        }
        Ok(())
    }
}

impl AudioController for PulseController {
    fn backend_name(&self) -> &'static str {
        "PulseAudio / PipeWire"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities { per_app_volume: true, per_app_mute: true }
    }

    fn list_audio_apps(&mut self) -> AudioResult<Vec<AudioApp>> {
        let mut grouped: BTreeMap<String, Vec<Input>> = BTreeMap::new();
        for i in self.inputs()? {
            grouped.entry(i.key.clone()).or_default().push(i);
        }
        let mut apps: Vec<AudioApp> = grouped
            .into_iter()
            .map(|(key, ins)| {
                let mut pids: Vec<u32> = ins.iter().map(|i| i.pid).filter(|p| *p != 0).collect();
                pids.dedup();
                AudioApp {
                    name: ins[0].name.clone(),
                    volume: Some(ins.iter().map(|i| i.volume).sum::<f32>() / ins.len() as f32),
                    muted: ins.iter().all(|i| i.muted),
                    exe_path: pids.first().and_then(|p| std::fs::read_link(format!("/proc/{p}/exe")).ok()),
                    pids,
                    key,
                }
            })
            .collect();
        apps.sort_by_key(|a| a.name.to_lowercase());
        Ok(apps)
    }

    fn set_app_volume(&mut self, app: &str, volume: f32) -> AudioResult<()> {
        let level = Volume((volume.clamp(0.0, 1.0) * Volume::NORMAL.0 as f32) as u32);
        self.for_each_input_of(app, |me, input| {
            let mut cv = ChannelVolumes::default();
            cv.set(input.channels.max(1), level);
            me.context.introspect().set_sink_input_volume(input.index, &cv, None)
        })
    }

    fn set_app_mute(&mut self, app: &str, muted: bool) -> AudioResult<()> {
        self.for_each_input_of(app, |me, input| me.context.introspect().set_sink_input_mute(input.index, muted, None))
    }
}
