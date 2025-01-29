use std::cell::Cell;
use std::cell::RefCell;
use std::cell::RefMut;
use std::rc::Rc;

use niri_ipc::SizeChange;
use pulse::callbacks::ListResult;
use pulse::context::introspect::Introspector;
use pulse::context::introspect::ServerInfo;
use pulse::context::introspect::SinkInfo;
use pulse::context::subscribe::Facility;
use pulse::context::subscribe::InterestMaskSet;
use pulse::context::subscribe::Operation;
use pulse::context::Context;
use pulse::context::FlagSet;
use pulse::context::State as ContextState;
use pulse::mainloop::threaded::Mainloop;
use pulse::proplist::properties;
use pulse::proplist::Proplist;
use pulse::volume::ChannelVolumes;
use pulse::volume::Volume;

use crate::niri::State;

/// PulseAudio internal state
struct Pai {
    // NOTE: Declare ctx field before mainloop
    // since context still holds a reference to mainloop
    // during cleanup time in some cases.
    ctx: RefCell<Context>,
    mainloop: RefCell<Mainloop>,
    sink_index: Cell<Option<u32>>,
    base_volume: Cell<Volume>,
    chan_volumes: Cell<ChannelVolumes>,
    mute: Cell<bool>,
}

/// PulseAudio shareable state
pub struct Pa(Rc<Pai>);

impl Clone for Pa {
    fn clone(&self) -> Self {
        Self(Rc::clone(&self.0))
    }
}

impl Pa {
    pub fn start(state: &mut State) {
        if let Some(_) = state.niri.pa {
            return;
        }

        let Some(pa) = Self::new() else {
            debug!("Failed to create pa_client");
            return;
        };

        if let Err(e) = pa.connect() {
            debug!("pa_context connect failed: {:?}", e);
            return;
        };

        debug!("pa_client is ready");
        state.niri.pa = Some(pa);
    }

    pub fn update_vol(state: &State, amount: SizeChange) {
        let Some(pa) = state.niri.pa.as_ref() else {
            trace!("pa_client is not available");
            return;
        };

        // There is a small window of data race when mainloop
        // is processing a change from default sink, and we are
        // poking shared data here. Those cases are rare.
        pa.mainloop_mut().lock();
        let i = pa.0.sink_index.get();
        pa.mainloop_mut().unlock();

        let Some(i) = i else {
            trace!("pa_client has no sink_index");
            return;
        };

        match amount {
            SizeChange::SetFixed(n) => {
                trace!("pa_volume = {}", n);
            }
            SizeChange::SetProportion(p) => {
                if p > 0.0 {
                    trace!("pa_volume ↑ {:.0}%", p);
                }
                if p < 0.0 {
                    trace!("pa_volume ↓ {:.0}%", p);
                }
            }
            SizeChange::AdjustFixed(n) => {
                if n > 0 {
                    trace!("pa_volume ↑ {}", n);
                }
                if n < 0 {
                    trace!("pa_volume ↓ {}", n);
                }
            }
            SizeChange::AdjustProportion(p) => {
                let p = p.min(100.0).max(-100.0);
                trace!("pa_volume {}, {:.0}%", i, p);

                pa.mainloop_mut().lock();
                let mut vols = pa.0.chan_volumes.get();
                let volbase = pa.0.base_volume.get();
                pa.mainloop_mut().unlock();

                let step = (volbase.0 as f64) / 100.0;
                let change = (p * step) as i32;
                if change == 0 {
                    return;
                } else if change < 0 {
                    vols.decrease(Volume(-change as u32));
                } else if change > 0 {
                    vols.inc_clamp(Volume((change) as u32), volbase);
                }

                // TODO: does this lock necessary?
                pa.mainloop_mut().lock();
                pa.introspect().set_sink_volume_by_index(i, &vols, None);
                pa.mainloop_mut().unlock();
            }
        };
    }

    pub fn toggle_mute(state: &State) {
        let Some(pa) = state.niri.pa.as_ref() else {
            return;
        };

        trace!("pa_toggle_mute");

        pa.mainloop_mut().lock();
        let set_mute = !pa.0.mute.get();
        if let Some(index) = pa.0.sink_index.get() {
            pa.introspect()
                .set_sink_mute_by_index(index, set_mute, None);
        }
        pa.mainloop_mut().unlock();
    }

    fn new() -> Option<Self> {
        let mainloop = Mainloop::new()?;
        let mut proplist = Proplist::new()?;
        proplist
            .set_str(properties::APPLICATION_NAME, "niri")
            .ok()?;
        let ctx = Context::new_with_proplist(&mainloop, "nirictx", &proplist)?;

        Some(Self(Rc::new(Pai {
            mainloop: RefCell::new(mainloop),
            ctx: RefCell::new(ctx),
            sink_index: Cell::new(None),
            base_volume: Cell::new(Volume(65535)),
            chan_volumes: Cell::new(ChannelVolumes::default()),
            mute: Cell::new(false),
        })))
    }

    #[inline]
    fn introspect(&self) -> Introspector {
        self.0.ctx.borrow().introspect()
    }

    #[inline]
    fn signal_loop_unsafe(&self) {
        unsafe {
            (*self.0.mainloop.as_ptr()).signal(false);
        }
    }

    #[inline]
    fn get_context_state_unsafe(&self) -> ContextState {
        unsafe { (*self.0.ctx.as_ptr()).get_state() }
    }

    #[inline]
    fn context_mut(&self) -> RefMut<'_, Context> {
        self.0.ctx.borrow_mut()
    }

    #[inline]
    fn mainloop_mut(&self) -> RefMut<'_, Mainloop> {
        self.0.mainloop.borrow_mut()
    }

    fn connect(&self) -> anyhow::Result<()> {
        let s = self.clone();
        self.context_mut()
            .set_state_callback(Some(Box::new(move || {
                let state = s.get_context_state_unsafe();
                match state {
                    ContextState::Failed | ContextState::Terminated | ContextState::Ready => {
                        s.signal_loop_unsafe();
                    }
                    _ => {}
                };
            })));

        let flags = FlagSet::NOFAIL | FlagSet::NOAUTOSPAWN;
        self.context_mut().connect(None, flags, None)?;

        self.mainloop_mut().lock();
        self.mainloop_mut().start()?;

        // Wait for context to be ready
        loop {
            match self.0.ctx.borrow().get_state() {
                ContextState::Ready => {
                    debug!("pa_context is ready");
                    break;
                }
                ContextState::Failed | ContextState::Terminated => {
                    self.mainloop_mut().unlock();
                    self.mainloop_mut().stop();
                    anyhow::bail!("pa_context failed during initial connect");
                }
                _ => {
                    self.mainloop_mut().wait();
                }
            }
        }
        self.context_mut().set_state_callback(None);
        self.mainloop_mut().unlock();

        let f = self._subscribe();
        self.context_mut().set_subscribe_callback(Some(Box::new(f)));
        self.context_mut().subscribe(InterestMaskSet::SINK, |_| {});

        self.introspect().get_server_info(self._server_info());

        Ok(())
    }

    fn _subscribe(&self) -> impl FnMut(Option<Facility>, Option<Operation>, u32) {
        let s = self.clone();
        move |f, op, index| {
            // we only work on sinks
            let Some(f) = f else {
                return;
            };

            if f != Facility::Sink {
                return;
            }

            let Some(op) = op else {
                return;
            };

            match op {
                Operation::Changed => {
                    let Some(i) = s.0.sink_index.get() else {
                        return;
                    };
                    if index != i {
                        return;
                    }
                    s.introspect().get_sink_info_by_index(index, s._sink_info());
                }
                _ => {
                    // server topology changed, should ask for new configuration
                    s.introspect().get_server_info(s._server_info());
                }
            }
        }
    }

    fn _server_info(&self) -> impl FnMut(&ServerInfo) {
        let s = self.clone();
        move |info| {
            let Some(name) = info.default_sink_name.as_ref() else {
                return;
            };
            s.introspect()
                .get_sink_info_by_name(name.as_ref(), s._sink_info());
        }
    }

    fn _sink_info(&self) -> impl FnMut(ListResult<&SinkInfo>) {
        let s = self.clone();
        move |lst| {
            let ListResult::Item(info) = lst else {
                return;
            };

            if let Some(i) = s.0.sink_index.get() {
                if i != info.index {
                    trace!("change default sink index {} -> {}", i, info.index);
                }
            } else {
                trace!("set default sink index to {}", info.index);
            }

            s.0.sink_index.set(Some(info.index));
            s.0.base_volume.set(info.base_volume);
            s.0.chan_volumes.set(info.volume.to_owned());
            s.0.mute.set(info.mute);
        }
    }
}
