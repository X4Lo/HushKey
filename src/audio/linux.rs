//! Linux backend: PulseAudio sink inputs (also works on PipeWire via `pipewire-pulse`,
//! the default on Fedora, Ubuntu 22.10+, Arch, etc.).
//!
//! Every playback stream an app opens is a *sink input*; that includes paused/corked
//! streams, so idle-but-registered apps show up too. Full per-app volume and mute are
//! natively supported. Output devices are *sinks*: system volume is the default sink's
//! volume, and per-app routing moves the app's sink inputs (PulseAudio's stream-restore
//! module, and PipeWire, remember the choice per app).
//!
//! Uses the single-threaded standard mainloop driven synchronously from the audio worker
//! thread: each call issues an operation and iterates the loop until it completes.

use super::{AudioApp, AudioController, AudioError, AudioResult, Capabilities, MasterVolume, OutputDevice};
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
    sink: u32,
}

#[derive(Clone)]
struct Sink {
    index: u32,
    name: String,
    description: String,
    volume: f32,
    muted: bool,
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
                    sink: i.sink,
                });
            }
        });
        self.wait(op)?;
        let v = out.borrow().clone();
        Ok(v)
    }

    fn sinks(&mut self) -> AudioResult<Vec<Sink>> {
        let out: Rc<RefCell<Vec<Sink>>> = Rc::default();
        let sink = out.clone();
        let op = self.context.introspect().get_sink_info_list(move |r| {
            if let ListResult::Item(i) = r {
                let name = i.name.as_deref().unwrap_or_default().to_owned();
                sink.borrow_mut().push(Sink {
                    index: i.index,
                    description: i.description.as_deref().filter(|d| !d.is_empty()).unwrap_or(&name).to_owned(),
                    name,
                    volume: i.volume.avg().0 as f32 / Volume::NORMAL.0 as f32,
                    muted: i.mute,
                    channels: i.volume.len(),
                });
            }
        });
        self.wait(op)?;
        let v = out.borrow().clone();
        Ok(v)
    }

    fn default_sink_name(&mut self) -> AudioResult<Option<String>> {
        let out: Rc<RefCell<Option<String>>> = Rc::default();
        let sink = out.clone();
        let op = self.context.introspect().get_server_info(move |info| {
            *sink.borrow_mut() = info.default_sink_name.as_deref().map(str::to_owned);
        });
        self.wait(op)?;
        let v = out.borrow().clone();
        Ok(v)
    }

    fn default_sink(&mut self) -> AudioResult<Sink> {
        let name = self.default_sink_name()?;
        let sinks = self.sinks()?;
        name.and_then(|n| sinks.iter().find(|s| s.name == n).cloned())
            .or_else(|| sinks.first().cloned())
            .ok_or_else(|| backend("no output device"))
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
        Capabilities {
            per_app_volume: true,
            per_app_mute: true,
            per_app_device: true,
            master_volume: true,
            output_devices: true,
        }
    }

    fn list_audio_apps(&mut self) -> AudioResult<Vec<AudioApp>> {
        let sinks = self.sinks().unwrap_or_default();
        let default_sink = self.default_sink_name().ok().flatten();
        let mut grouped: BTreeMap<String, Vec<Input>> = BTreeMap::new();
        for i in self.inputs()? {
            grouped.entry(i.key.clone()).or_default().push(i);
        }
        let mut apps: Vec<AudioApp> = grouped
            .into_iter()
            .map(|(key, ins)| {
                let mut pids: Vec<u32> = ins.iter().map(|i| i.pid).filter(|p| *p != 0).collect();
                pids.dedup();
                // PulseAudio doesn't say whether a stream *follows* the default; treat a
                // stream on the default sink as following it, anything else as routed.
                let device = sinks
                    .iter()
                    .find(|s| s.index == ins[0].sink)
                    .map(|s| s.name.clone())
                    .filter(|name| Some(name) != default_sink.as_ref());
                AudioApp {
                    device,
                    routable: true,
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

    fn output_devices(&mut self) -> AudioResult<Vec<OutputDevice>> {
        let default = self.default_sink_name()?;
        let mut out: Vec<OutputDevice> = self
            .sinks()?
            .into_iter()
            .map(|s| OutputDevice { is_default: Some(&s.name) == default.as_ref(), id: s.name, name: s.description })
            .collect();
        out.sort_by_key(|d| d.name.to_lowercase());
        Ok(out)
    }

    fn set_default_output_device(&mut self, id: &str) -> AudioResult<()> {
        let op = self.context.set_default_sink(id, |_| {});
        self.wait(op)
    }

    fn master_volume(&mut self) -> AudioResult<MasterVolume> {
        let s = self.default_sink()?;
        Ok(MasterVolume { volume: Some(s.volume), muted: s.muted })
    }

    fn set_master_volume(&mut self, volume: f32) -> AudioResult<()> {
        let s = self.default_sink()?;
        let mut cv = ChannelVolumes::default();
        cv.set(s.channels.max(1), Volume((volume.clamp(0.0, 1.0) * Volume::NORMAL.0 as f32) as u32));
        let op = self.context.introspect().set_sink_volume_by_name(&s.name, &cv, None);
        self.wait(op)
    }

    fn set_master_mute(&mut self, muted: bool) -> AudioResult<()> {
        let s = self.default_sink()?;
        let op = self.context.introspect().set_sink_mute_by_name(&s.name, muted, None);
        self.wait(op)
    }

    fn set_app_output_device(&mut self, app: &str, device: Option<&str>) -> AudioResult<()> {
        let target = match device {
            Some(d) => d.to_owned(),
            None => self.default_sink()?.name,
        };
        self.for_each_input_of(app, |me, input| {
            me.context.introspect().move_sink_input_by_name(input.index, &target, None)
        })
    }
}
