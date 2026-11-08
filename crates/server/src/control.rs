//! Run-control requests arriving over the data channel. The session belongs to
//! the DAP connection when there is one, so requests are handed to its thread.

use std::path::PathBuf;
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};

use engine::session::{RunState, Session};
use probe_api::Progress;
use sampler::Sampler;
use serde_json::{json, Value};
use tsdb::EventKind;

use crate::dap::binding_of;

pub enum Control {
    Halt,
    Go,
    Reset,
    Reload { elf: Option<PathBuf> },
}

pub type ControlReply = Sender<Result<Value, String>>;

/// What the DAP connection's loop receives.
pub enum Incoming {
    Dap(Value),
    Control(Control, ControlReply),
}

/// Reaches whoever currently owns the session.
#[derive(Default)]
pub struct ControlBus {
    controller: Mutex<Option<Sender<Incoming>>>,
    idle: Mutex<Option<IdleSession>>,
}

struct IdleSession {
    session: Arc<Mutex<Option<Session>>>,
    sampler: Arc<Sampler>,
}

impl ControlBus {
    pub fn new(session: Arc<Mutex<Option<Session>>>, sampler: Arc<Sampler>) -> Arc<Self> {
        let bus = Self::default();
        *bus.idle.lock().expect("bus lock") = Some(IdleSession { session, sampler });
        Arc::new(bus)
    }

    pub fn set_controller(&self, sender: Option<Sender<Incoming>>) {
        *self.controller.lock().expect("bus lock") = sender;
    }

    pub fn has_controller(&self) -> bool {
        self.controller.lock().expect("bus lock").is_some()
    }

    pub fn send(&self, command: Control, reply: ControlReply) {
        let controller = self.controller.lock().expect("bus lock").clone();
        if let Some(controller) = controller {
            let _ = controller.send(Incoming::Control(command, reply));
            return;
        }
        let idle = self.idle.lock().expect("bus lock");
        let Some(idle) = idle.as_ref() else {
            let _ = reply.send(Err("no target is connected".to_owned()));
            return;
        };
        let mut session = idle.session.lock().expect("session lock");
        let result = match session.as_mut() {
            Some(session) => run_idle(session, &idle.sampler, command),
            None => Err("no target is connected".to_owned()),
        };
        let _ = reply.send(result);
    }
}

/// Serve a request with no debug client attached.
fn run_idle(
    session: &mut Session,
    sampler: &Arc<Sampler>,
    command: Control,
) -> Result<Value, String> {
    let fail = |error: engine::session::EngineError| error.to_string();
    match command {
        Control::Halt => {
            if *session.state() == RunState::Running {
                session.pause().map_err(fail)?;
                sampler.note_event(EventKind::Halted, json!({"reason": "pause"}));
            }
        }
        Control::Go => {
            session.resume().map_err(fail)?;
            sampler.note_event(EventKind::Resumed, json!({}));
        }
        Control::Reset => {
            session.reset_halt().map_err(fail)?;
            session.resume().map_err(fail)?;
            sampler.note_event(EventKind::Reset, json!({}));
        }
        Control::Reload { elf } => {
            sampler.unbind();
            let result = session.reload(elf, |_: Progress| {});
            let resumed = result.is_ok() && session.resume().is_ok();
            sampler.bind(binding_of(session));
            result.map_err(fail)?;
            if !resumed {
                return Err("the image loaded but the target did not start".to_owned());
            }
            sampler.note_event(EventKind::Reload, json!({}));
        }
    }
    Ok(json!({}))
}
