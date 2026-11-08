//! The long-running process. It owns the probe across debug sessions so that
//! neither the connection nor the symbol index is rebuilt each time, and keeps
//! sampling for the data channel in between.

use std::io::{self, BufReader};
use std::net::{Ipv4Addr, Shutdown, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use engine::session::Session;
use rtt::Rtt;
use sampler::Sampler;
use serde_json::json;
use tsdb::Tsdb;

use crate::control::{ControlBus, Incoming};
use crate::dap::{Client, Dap};
use crate::http;
use crate::protocol::read_message;
use crate::rpc::App;

/// How often a running target is checked for a stop (DESIGN §5.5.1).
const POLL_INTERVAL: Duration = Duration::from_millis(20);
const IDLE_CHECK: Duration = Duration::from_millis(500);

struct Shared {
    session: Arc<Mutex<Option<Session>>>,
    sampler: Arc<Sampler>,
    control: Arc<ControlBus>,
    controller_active: AtomicBool,
}

/// What an idle backend should do next.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum IdleAction {
    Stay,
    ReleaseProbe,
    Exit,
}

/// Tracks how long the backend has gone unused. The probe is let go once no
/// debug session has needed it for `limit`. The process itself stays while a
/// data client is connected, since the recorded data lives here.
struct IdleTracker {
    limit: Duration,
    last_controller: Instant,
    last_client: Instant,
}

impl IdleTracker {
    fn new(limit: Duration, now: Instant) -> Self {
        Self {
            limit,
            last_controller: now,
            last_client: now,
        }
    }

    fn update(&mut self, controller: bool, clients: usize, now: Instant) -> IdleAction {
        if controller {
            self.last_controller = now;
        }
        if clients > 0 {
            self.last_client = now;
        }
        let unneeded = now.duration_since(self.last_controller) >= self.limit;
        if unneeded && now.duration_since(self.last_client) >= self.limit {
            IdleAction::Exit
        } else if unneeded {
            IdleAction::ReleaseProbe
        } else {
            IdleAction::Stay
        }
    }
}

fn release_probe(shared: &Shared) {
    let mut session = shared.session.lock().expect("session lock");
    // Checked under the lock: a new session takes it before using the probe.
    if shared.controller_active.load(Ordering::SeqCst) {
        return;
    }
    if let Some(open) = session.take() {
        shared.sampler.unbind();
        if let Err(error) = open.shutdown() {
            tracing::warn!("closing the probe failed: {error}");
        }
        tracing::info!("released the probe after being idle");
    }
}

fn watch_for_idle(shared: Arc<Shared>, app: Arc<App>, limit: Duration) {
    let mut tracker = IdleTracker::new(limit, Instant::now());
    loop {
        thread::sleep(IDLE_CHECK);
        let controller = shared.controller_active.load(Ordering::SeqCst);
        match tracker.update(controller, app.client_count(), Instant::now()) {
            IdleAction::Stay => {}
            IdleAction::ReleaseProbe => release_probe(&shared),
            IdleAction::Exit => {
                release_probe(&shared);
                tracing::info!("exiting after being idle");
                std::process::exit(0);
            }
        }
    }
}

/// Serve the web UI and data channel, and DAP clients. With `idle_limit` set,
/// exit after being unused for that long; the next debug session starts a new
/// process.
pub fn serve(http_port: u16, dap_port: u16, idle_limit: Option<Duration>) -> io::Result<()> {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, dap_port))?;
    tracing::info!("listening for DAP clients on 127.0.0.1:{dap_port}");
    let session = Arc::new(Mutex::new(None));
    let sampler = Sampler::new(Arc::new(Tsdb::default()));
    let rtt = Rtt::new(sampler.tsdb().clone());
    sampler.add_listener(rtt.clone());
    let control = ControlBus::new(session.clone(), sampler.clone());
    let shared = Arc::new(Shared {
        session,
        sampler: sampler.clone(),
        control: control.clone(),
        controller_active: AtomicBool::new(false),
    });
    let app = App::new(sampler, rtt, control);
    if let Some(limit) = idle_limit {
        let shared = shared.clone();
        let app = app.clone();
        thread::Builder::new()
            .name("idle".to_owned())
            .spawn(move || watch_for_idle(shared, app, limit))?;
    }
    thread::Builder::new()
        .name("http".to_owned())
        .spawn(move || {
            if let Err(error) = http::run(app, http_port) {
                tracing::error!("the web server stopped: {error}");
            }
        })?;

    for stream in listener.incoming() {
        let stream = stream?;
        let shared = shared.clone();
        thread::spawn(move || {
            if let Err(error) = handle_connection(stream, &shared) {
                tracing::debug!("connection ended: {error}");
            }
        });
    }
    Ok(())
}

fn reject(stream: TcpStream) -> io::Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let client = Client::new(stream);
    if let Some(request) = read_message(&mut reader)? {
        client.respond(
            &request,
            Err("another debug session is already controlling the target".to_owned()),
        );
    }
    Ok(())
}

fn handle_connection(stream: TcpStream, shared: &Shared) -> io::Result<()> {
    if shared.controller_active.swap(true, Ordering::SeqCst) {
        return reject(stream);
    }
    let result = run_controller(stream, shared);
    shared.control.set_controller(None);
    shared.controller_active.store(false, Ordering::SeqCst);
    result
}

fn run_controller(stream: TcpStream, shared: &Shared) -> io::Result<()> {
    stream.set_nodelay(true)?;
    let mut reader = BufReader::new(stream.try_clone()?);
    let cancel = Arc::new(AtomicBool::new(false));
    // The reader thread holds its own handle, so dropping ours would not close
    // the connection and the editor would wait for the adapter to exit.
    let closer = stream.try_clone()?;
    let client = Client::new(stream);
    let session = shared.session.lock().expect("session lock").take();
    let mut dap = Dap::new(client, session, cancel.clone(), shared.sampler.clone());

    // Requests are read on their own thread so `pause` can interrupt a step.
    let (sender, receiver) = mpsc::channel::<Incoming>();
    shared.control.set_controller(Some(sender.clone()));
    let reader_cancel = cancel;
    thread::spawn(move || {
        while let Ok(Some(message)) = read_message(&mut reader) {
            let command = &message["command"];
            if command == "pause" || command == "terminate" || command == "disconnect" {
                reader_cancel.store(true, Ordering::SeqCst);
            }
            if sender.send(Incoming::Dap(message)).is_err() {
                return;
            }
        }
        // The client is gone: abandon whatever the engine is waiting for.
        reader_cancel.store(true, Ordering::SeqCst);
        let _ = sender.send(Incoming::Dap(json!({"command": "<closed>"})));
    });

    loop {
        match receiver.recv_timeout(POLL_INTERVAL) {
            Ok(Incoming::Dap(request)) => {
                if request["command"] == json!("<closed>") {
                    dap.abandon();
                    break;
                }
                if !dap.handle(&request) {
                    break;
                }
            }
            Ok(Incoming::Control(command, reply)) => {
                let _ = reply.send(dap.control(command));
            }
            Err(RecvTimeoutError::Timeout) => {
                if dap.is_running() {
                    dap.poll();
                }
            }
            Err(RecvTimeoutError::Disconnected) => {
                dap.abandon();
                break;
            }
        }
    }
    *shared.session.lock().expect("session lock") = dap.into_session();
    let _ = closer.shutdown(Shutdown::Both);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIMIT: Duration = Duration::from_secs(30);

    #[test]
    fn a_debug_session_keeps_everything() {
        let start = Instant::now();
        let mut tracker = IdleTracker::new(LIMIT, start);
        assert_eq!(tracker.update(true, 0, start + LIMIT * 2), IdleAction::Stay);
    }

    #[test]
    fn with_nothing_connected_the_process_exits_after_the_limit() {
        let start = Instant::now();
        let mut tracker = IdleTracker::new(LIMIT, start);
        assert_eq!(
            tracker.update(false, 0, start + LIMIT - Duration::from_secs(1)),
            IdleAction::Stay
        );
        assert_eq!(tracker.update(false, 0, start + LIMIT), IdleAction::Exit);
    }

    #[test]
    fn an_open_timeline_keeps_the_data_but_not_the_probe() {
        let start = Instant::now();
        let mut tracker = IdleTracker::new(LIMIT, start);
        assert_eq!(
            tracker.update(false, 1, start + LIMIT),
            IdleAction::ReleaseProbe
        );
        // Closing the page starts its own countdown.
        let closed = start + LIMIT;
        assert_eq!(
            tracker.update(false, 0, closed + Duration::from_secs(1)),
            IdleAction::ReleaseProbe
        );
        assert_eq!(tracker.update(false, 0, closed + LIMIT), IdleAction::Exit);
    }

    #[test]
    fn a_session_restarts_the_countdown() {
        let start = Instant::now();
        let mut tracker = IdleTracker::new(LIMIT, start);
        let ended = start + LIMIT * 3;
        assert_eq!(tracker.update(true, 0, ended), IdleAction::Stay);
        assert_eq!(
            tracker.update(false, 0, ended + LIMIT / 2),
            IdleAction::Stay
        );
    }
}
