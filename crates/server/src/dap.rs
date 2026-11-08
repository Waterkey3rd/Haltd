//! Debug Adapter Protocol request handling (DESIGN §6.1).

use std::collections::HashSet;
use std::io::{BufWriter, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use engine::breakpoints::ResolvedBreakpoint;
use engine::project::ProjectConfig;
use engine::session::{
    DisplayFrame, LaunchOptions, RunState, Session, StopReason, VariableView, MAIN_THREAD,
};
use engine::step::StepKind;
use probe_api::{ConnectCfg, Interface, Probe, Progress, DEFAULT_SPEED_KHZ};
use probe_jlink::actor::Priority;
use probe_jlink::{JLink, JLinkOptions};
use sampler::codec::joined;
use sampler::{Binding, Sampler, SamplingConfig};
use serde_json::{json, Value};
use symbols::Place;
use tsdb::EventKind;

use crate::control::Control;
use crate::protocol::write_message;

type Handled = Result<Value, String>;

fn fail(error: impl std::fmt::Display) -> String {
    error.to_string()
}

/// Sends messages back to the client from any thread.
#[derive(Clone)]
pub struct Client {
    writer: Arc<Mutex<(BufWriter<TcpStream>, i64)>>,
}

impl Client {
    pub fn new(stream: TcpStream) -> Self {
        Self {
            writer: Arc::new(Mutex::new((BufWriter::new(stream), 1))),
        }
    }

    fn send(&self, mut message: Value) {
        let mut guard = self.writer.lock().expect("writer lock");
        let (writer, seq) = &mut *guard;
        message["seq"] = json!(*seq);
        *seq += 1;
        if let Err(error) = write_message(writer, &message).and_then(|()| writer.flush()) {
            tracing::debug!("client write failed: {error}");
        }
    }

    pub fn event(&self, name: &str, body: Value) {
        self.send(json!({"type": "event", "event": name, "body": body}));
    }

    pub fn respond(&self, request: &Value, result: Handled) {
        let mut response = json!({
            "type": "response",
            "request_seq": request["seq"],
            "command": request["command"],
            "success": result.is_ok(),
        });
        match result {
            Ok(body) => response["body"] = body,
            Err(message) => response["message"] = json!(message),
        }
        self.send(response);
    }

    pub fn console(&self, text: &str) {
        self.event(
            "output",
            json!({"category": "console", "output": format!("{text}\n")}),
        );
    }
}

enum VarNode {
    Locals {
        thread: i64,
        frame: usize,
    },
    Registers {
        thread: i64,
        frame: usize,
    },
    Globals,
    Children {
        place: Place,
        synthetic: Arc<Vec<u8>>,
        expr: Option<String>,
    },
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum AfterConfiguration {
    Run,
    RunToMain,
    StopOnEntry,
    /// Attached: report the current state and leave it alone.
    Observe,
}

/// One connected client.
pub struct Dap {
    client: Client,
    pub session: Option<Session>,
    cancel: Arc<AtomicBool>,
    frames: Vec<(i64, usize)>,
    variables: Vec<VarNode>,
    after_configuration: AfterConfiguration,
    last_threads: Vec<Value>,
    run_to_main_on_reload: bool,
    sampler: Arc<Sampler>,
    /// Whether the Timeline currently shows the target as running.
    shown_running: bool,
    /// The client has ended debugging, by terminate or disconnect.
    finished: bool,
}

/// What the sampler needs to know about a connected target.
pub(crate) fn binding_of(session: &Session) -> Binding {
    let options = session.options();
    let sampling = &options.project.sampling;
    let probe = session
        .probe_handle()
        .call_blocking(Priority::P4, |probe: &mut dyn Probe| probe.info())
        .unwrap_or_default();
    let memory = session.device_profile().map_or_else(Vec::new, |profile| {
        profile
            .memory
            .iter()
            .map(|region| {
                (
                    region.start,
                    region.start.saturating_add(region.size),
                    region.kind == "ram",
                )
            })
            .collect()
    });
    Binding {
        handle: session.probe_handle(),
        db: session.symbols().clone(),
        device: options.connect.device.clone(),
        elf: options.elf.clone(),
        speed_khz: session.target().speed_khz,
        probe,
        memory,
        visualizer_files: options
            .project
            .visualizers
            .iter()
            .map(|file| match &options.workspace {
                Some(workspace) => workspace.join(file),
                None => PathBuf::from(file),
            })
            .collect(),
        config: SamplingConfig {
            hss_period_us: sampling.hss.period_us,
            watch_rate_hz: sampling.watch_rate_hz,
        },
        rtt_config: match (&options.project.rtt.config, &options.workspace) {
            (Some(file), Some(workspace)) => Some(workspace.join(file)),
            (Some(file), None) => Some(PathBuf::from(file)),
            (None, Some(workspace)) => rtt::default_config_path(workspace),
            (None, None) => None,
        },
        workspace: options.workspace.clone(),
    }
}

fn stop_body(reason: &StopReason, thread: i64) -> Value {
    let (name, breakpoints, description) = match reason {
        StopReason::Breakpoint(ids) => ("breakpoint", ids.clone(), None),
        StopReason::Step => ("step", Vec::new(), None),
        StopReason::Pause | StopReason::External => ("pause", Vec::new(), None),
        StopReason::Entry => ("entry", Vec::new(), None),
        StopReason::Data => ("data breakpoint", Vec::new(), None),
        StopReason::Fault => ("exception", Vec::new(), Some("Fault vector catch")),
    };
    let mut body = json!({
        "reason": name,
        "threadId": thread,
        "allThreadsStopped": true,
    });
    if !breakpoints.is_empty() {
        body["hitBreakpointIds"] = json!(breakpoints);
    }
    if let Some(text) = description {
        body["description"] = json!(text);
        body["text"] = json!(text);
    }
    body
}

fn describe_breakpoint(breakpoint: &ResolvedBreakpoint) -> Value {
    let mut out = json!({
        "id": breakpoint.id,
        "verified": breakpoint.verified,
        "line": breakpoint.line,
    });
    if let Some(file) = &breakpoint.file {
        out["source"] = json!({"path": file});
    }
    if let Some(message) = &breakpoint.message {
        out["message"] = json!(message);
    }
    out
}

fn parse_interface(arguments: &Value) -> Result<Interface, String> {
    match arguments["interface"].as_str().unwrap_or("SWD") {
        text if text.eq_ignore_ascii_case("swd") => Ok(Interface::Swd),
        text if text.eq_ignore_ascii_case("jtag") => Ok(Interface::Jtag),
        other => Err(format!("unknown interface {other:?}")),
    }
}

/// `speed` is a number of kHz, or "auto" for the default rate.
fn parse_speed(arguments: &Value) -> Result<u32, String> {
    match &arguments["speed"] {
        Value::Null => Ok(DEFAULT_SPEED_KHZ),
        Value::Number(number) => number
            .as_u64()
            .map(|value| value as u32)
            .ok_or_else(|| format!("invalid speed {number}")),
        Value::String(text) if text == "auto" => Ok(DEFAULT_SPEED_KHZ),
        Value::String(text) => text.parse().map_err(|_| format!("invalid speed {text:?}")),
        other => Err(format!("invalid speed {other}")),
    }
}

fn launch_options(arguments: &Value) -> Result<LaunchOptions, String> {
    let elf = arguments["elf"]
        .as_str()
        .ok_or("the launch configuration needs \"elf\"")?;
    let project = match arguments["project"].as_str() {
        Some(path) if std::path::Path::new(path).exists() => {
            ProjectConfig::load(std::path::Path::new(path))?
        }
        _ => ProjectConfig::default(),
    };
    Ok(LaunchOptions {
        connect: ConnectCfg {
            device: arguments["device"]
                .as_str()
                .ok_or("the launch configuration needs \"device\"")?
                .to_owned(),
            interface: parse_interface(arguments)?,
            speed_khz: parse_speed(arguments)?,
        },
        elf: PathBuf::from(elf),
        project,
        // A project file lives at <workspace>/.jlinkd/project.yaml.
        workspace: arguments["project"]
            .as_str()
            .and_then(|path| std::path::Path::new(path).parent()?.parent())
            .map(PathBuf::from),
    })
}

fn variable_json(view: VariableView, reference: i64, expression: Option<String>) -> Value {
    let mut out = json!({
        "name": view.name,
        "value": view.value,
        "type": view.type_name,
        "variablesReference": reference,
    });
    if let Some(expression) = expression {
        out["evaluateName"] = json!(expression);
    }
    out
}

/// Expression for a tree entry, if there is one the evaluator can parse.
fn child_expression(parent: Option<&str>, name: &str) -> Option<String> {
    let expression = match parent {
        None => name.to_owned(),
        Some(parent) if name == "*" => format!("(*{parent})"),
        Some(parent) => joined(parent, name),
    };
    symbols::parse(&expression).is_ok().then_some(expression)
}

fn base_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

impl Dap {
    pub fn new(
        client: Client,
        session: Option<Session>,
        cancel: Arc<AtomicBool>,
        sampler: Arc<Sampler>,
    ) -> Self {
        let mut dap = Self {
            client,
            session,
            cancel,
            frames: Vec::new(),
            variables: Vec::new(),
            after_configuration: AfterConfiguration::Observe,
            last_threads: Vec::new(),
            run_to_main_on_reload: false,
            sampler,
            shown_running: false,
            finished: false,
        };
        if let Some(session) = &mut dap.session {
            session.set_cancel_token(dap.cancel.clone());
        }
        dap
    }

    fn session(&mut self) -> Result<&mut Session, String> {
        self.session
            .as_mut()
            .ok_or_else(|| "no target is connected".to_owned())
    }

    fn forget_handles(&mut self) {
        self.frames.clear();
        self.variables.clear();
    }

    fn add_var(&mut self, node: VarNode) -> i64 {
        self.variables.push(node);
        self.variables.len() as i64
    }

    fn current_thread(&mut self) -> i64 {
        self.session
            .as_mut()
            .and_then(|session| session.current_thread().ok())
            .unwrap_or(MAIN_THREAD)
    }

    fn note_running(&mut self) {
        if !self.shown_running {
            self.shown_running = true;
            self.sampler.note_event(EventKind::Resumed, json!({}));
        }
    }

    fn note_halted(&mut self, reason: &StopReason) {
        if matches!(reason, StopReason::Fault) {
            self.sampler.note_event(EventKind::Fault, json!({}));
        }
        if std::mem::take(&mut self.shown_running) {
            self.sampler
                .note_event(EventKind::Halted, json!({"reason": format!("{reason:?}")}));
        }
    }

    fn rebind_sampler(&mut self) {
        if let Some(session) = &self.session {
            self.sampler.bind(binding_of(session));
            self.client.event("jlinkd/bound", json!({}));
        }
    }

    fn report_stop(&mut self, reason: &StopReason) {
        self.forget_handles();
        self.note_halted(reason);
        self.sampler.note_stopped();
        let thread = self.current_thread();
        self.client.event("stopped", stop_body(reason, thread));
    }

    /// Check for a stop while the target runs.
    pub fn poll(&mut self) {
        let Some(session) = self.session.as_mut() else {
            return;
        };
        match session.poll() {
            Ok(Some(reason)) => self.report_stop(&reason),
            Ok(None) => {}
            Err(error) => {
                tracing::warn!("polling the target failed: {error}");
                self.client.console(&format!("probe error: {error}"));
                if let Some(session) = self.session.as_mut() {
                    session.mark_halted();
                }
            }
        }
    }

    pub fn is_running(&self) -> bool {
        self.session
            .as_ref()
            .is_some_and(|session| *session.state() == RunState::Running)
    }

    /// Handle one request. Returns false when the connection should end.
    pub fn handle(&mut self, request: &Value) -> bool {
        let command = request["command"].as_str().unwrap_or("");
        let arguments = &request["arguments"];
        let result = match command {
            "initialize" => self.initialize(),
            "launch" => self.launch(arguments),
            "attach" => self.attach(arguments),
            "configurationDone" => {
                // Reply first: running to main can take a while.
                self.client.respond(request, Ok(json!({})));
                self.configuration_done();
                return true;
            }
            "setBreakpoints" => self.set_breakpoints(arguments),
            "setFunctionBreakpoints" => self.set_function_breakpoints(arguments),
            "setExceptionBreakpoints" => Ok(json!({})),
            "threads" => self.threads(),
            "stackTrace" => self.stack_trace(arguments),
            "scopes" => self.scopes(arguments),
            "variables" => self.variables(arguments),
            "evaluate" => self.evaluate(arguments),
            "continue" => self.resume(),
            "next" => return self.step(request, StepKind::Over),
            "stepIn" => return self.step(request, StepKind::Into),
            "stepOut" => return self.step(request, StepKind::Out),
            "pause" => self.pause(),
            "restart" | "jlinkd/reload" => self.reload(arguments),
            // VS Code's Stop sends terminate and waits for the terminated event;
            // only then does it send disconnect.
            "terminate" => {
                let result = self.finish_debugging(false);
                self.client.respond(request, result);
                self.client.event("terminated", json!({}));
                return true;
            }
            "disconnect" => {
                let halt = arguments["terminateDebuggee"].as_bool().unwrap_or(false);
                let result = self.finish_debugging(halt);
                self.client.respond(request, result);
                return false;
            }
            other => Err(format!("unsupported request {other:?}")),
        };
        self.client.respond(request, result);
        true
    }

    fn initialize(&mut self) -> Handled {
        Ok(json!({
            "supportsConfigurationDoneRequest": true,
            "supportsFunctionBreakpoints": true,
            "supportsEvaluateForHovers": true,
            "supportsRestartRequest": true,
            "supportsTerminateRequest": true,
        }))
    }

    /// Reuse a kept-alive connection when the settings allow it.
    fn prepare_session(&mut self, options: LaunchOptions) -> Result<(), String> {
        if self.session.as_ref().is_some_and(|s| s.matches(&options)) {
            return self.session()?.reconfigure(options).map_err(fail);
        }
        if let Some(old) = self.session.take() {
            old.shutdown().map_err(fail)?;
        }
        let probe = JLink::open(&JLinkOptions::default()).map_err(fail)?;
        let mut session = Session::start(Box::new(probe), options).map_err(fail)?;
        session.set_cancel_token(self.cancel.clone());
        self.session = Some(session);
        Ok(())
    }

    fn progress(&self) -> impl Fn(Progress) + Send + 'static {
        let client = self.client.clone();
        let seen = Mutex::new(HashSet::new());
        move |progress: Progress| {
            // The library reports per sector and percent; once per phase is plenty.
            if seen
                .lock()
                .expect("progress lock")
                .insert(progress.action.clone())
            {
                client.console(&format!("{}...", progress.action));
            }
        }
    }

    fn launch(&mut self, arguments: &Value) -> Handled {
        self.sampler.unbind();
        self.prepare_session(launch_options(arguments)?)?;
        let flash = arguments["flash"].as_str().unwrap_or("changed");
        if flash != "never" {
            let progress = self.progress();
            let elapsed = self.session()?.flash(progress).map_err(fail)?;
            self.client
                .console(&format!("Download finished in {elapsed:.2?}"));
        }
        self.session()?.reset_halt().map_err(fail)?;
        self.run_to_main_on_reload = arguments["runToMain"].as_bool().unwrap_or(false);
        self.after_configuration = if arguments["stopOnEntry"].as_bool().unwrap_or(false) {
            AfterConfiguration::StopOnEntry
        } else if self.run_to_main_on_reload {
            AfterConfiguration::RunToMain
        } else {
            AfterConfiguration::Run
        };
        self.client.event("initialized", json!({}));
        Ok(json!({}))
    }

    fn attach(&mut self, arguments: &Value) -> Handled {
        self.sampler.unbind();
        self.prepare_session(launch_options(arguments)?)?;
        self.after_configuration = AfterConfiguration::Observe;
        self.client.event("initialized", json!({}));
        Ok(json!({}))
    }

    fn apply_start(&mut self, mode: AfterConfiguration) -> Result<(), String> {
        match mode {
            AfterConfiguration::Run => {
                self.session()?.resume().map_err(fail)?;
                self.note_running();
                Ok(())
            }
            AfterConfiguration::RunToMain => {
                let reason = self.session()?.run_to_main().map_err(fail)?;
                self.report_stop(&reason);
                Ok(())
            }
            AfterConfiguration::StopOnEntry => {
                self.report_stop(&StopReason::Entry);
                Ok(())
            }
            AfterConfiguration::Observe => {
                if !self.is_running() {
                    self.report_stop(&StopReason::Pause);
                }
                Ok(())
            }
        }
    }

    fn configuration_done(&mut self) {
        let mode = self.after_configuration;
        if let Err(error) = self.apply_start(mode) {
            self.client.console(&format!("cannot start: {error}"));
            self.client.event("terminated", json!({}));
            return;
        }
        if mode == AfterConfiguration::Observe && self.is_running() {
            self.note_running();
        }
        // Sampling starts only once the target is in its intended run state.
        self.rebind_sampler();
    }

    fn set_breakpoints(&mut self, arguments: &Value) -> Handled {
        let path = arguments["source"]["path"]
            .as_str()
            .ok_or("setBreakpoints needs a source path")?;
        let lines: Vec<u32> = arguments["breakpoints"]
            .as_array()
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| item["line"].as_u64())
                    .map(|line| line as u32)
                    .collect()
            })
            .unwrap_or_default();
        let resolved = self
            .session()?
            .set_source_breakpoints(path, &lines)
            .map_err(fail)?;
        let items: Vec<Value> = resolved.iter().map(describe_breakpoint).collect();
        Ok(json!({"breakpoints": items}))
    }

    fn set_function_breakpoints(&mut self, arguments: &Value) -> Handled {
        let names: Vec<String> = arguments["breakpoints"]
            .as_array()
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| item["name"].as_str())
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default();
        let resolved = self
            .session()?
            .set_function_breakpoints(&names)
            .map_err(fail)?;
        let items: Vec<Value> = resolved.iter().map(describe_breakpoint).collect();
        Ok(json!({"breakpoints": items}))
    }

    fn threads(&mut self) -> Handled {
        let running = self.is_running();
        if running && !self.last_threads.is_empty() {
            return Ok(json!({"threads": self.last_threads}));
        }
        let tasks = if running {
            None
        } else {
            self.session()?.tasks().map_err(fail)?
        };
        let threads: Vec<Value> = match tasks {
            Some(tasks) if !tasks.is_empty() => tasks
                .iter()
                .map(|task| {
                    json!({
                        "id": task.tcb,
                        "name": format!("{} [{}, {}]", task.name, task.state.name(), task.priority),
                    })
                })
                .collect(),
            _ => vec![json!({"id": MAIN_THREAD, "name": "main"})],
        };
        self.last_threads.clone_from(&threads);
        Ok(json!({"threads": threads}))
    }

    fn frame_json(&mut self, thread: i64, index: usize, frame: &DisplayFrame) -> Value {
        self.frames.push((thread, index));
        let mut out = json!({
            "id": self.frames.len(),
            "name": frame.name,
            "line": frame.location.as_ref().map_or(0, |l| l.line),
            "column": 1,
            "instructionPointerReference": format!("{:#010x}", frame.pc),
        });
        if let Some(location) = &frame.location {
            out["source"] = json!({
                "name": base_name(&location.file),
                "path": location.file,
            });
        }
        if frame.is_exception {
            out["presentationHint"] = json!("label");
        } else if frame.is_inlined {
            out["name"] = json!(format!("{} [inlined]", frame.name));
        } else if frame.location.is_none() {
            out["presentationHint"] = json!("subtle");
        }
        out
    }

    fn stack_trace(&mut self, arguments: &Value) -> Handled {
        if self.is_running() {
            return Err("the target is running".to_owned());
        }
        let thread = arguments["threadId"].as_i64().unwrap_or(MAIN_THREAD);
        let frames = self.session()?.stack_trace(thread).map_err(fail)?;
        let start = arguments["startFrame"].as_u64().unwrap_or(0) as usize;
        let levels = match arguments["levels"].as_u64() {
            Some(0) | None => frames.len(),
            Some(levels) => levels as usize,
        };
        let total = frames.len();
        let items: Vec<Value> = frames
            .iter()
            .enumerate()
            .skip(start)
            .take(levels)
            .map(|(index, frame)| self.frame_json(thread, index, frame))
            .collect();
        Ok(json!({"stackFrames": items, "totalFrames": total}))
    }

    fn frame_of(&self, id: Option<i64>) -> Option<(i64, usize)> {
        let index = usize::try_from(id? - 1).ok()?;
        self.frames.get(index).copied()
    }

    fn scopes(&mut self, arguments: &Value) -> Handled {
        let (thread, frame) = self
            .frame_of(arguments["frameId"].as_i64())
            .ok_or("unknown frame")?;
        let locals = self.add_var(VarNode::Locals { thread, frame });
        let registers = self.add_var(VarNode::Registers { thread, frame });
        let globals = self.add_var(VarNode::Globals);
        Ok(json!({"scopes": [
            {"name": "Locals", "presentationHint": "locals", "variablesReference": locals, "expensive": false},
            {"name": "Registers", "presentationHint": "registers", "variablesReference": registers, "expensive": false},
            {"name": "Globals", "variablesReference": globals, "expensive": true},
        ]}))
    }

    fn child_reference(
        &mut self,
        view: &VariableView,
        synthetic: &Arc<Vec<u8>>,
        expr: Option<String>,
    ) -> i64 {
        match (view.expandable, view.place) {
            (true, Some(place)) => self.add_var(VarNode::Children {
                place,
                synthetic: synthetic.clone(),
                expr,
            }),
            _ => 0,
        }
    }

    fn views_json(
        &mut self,
        views: Vec<VariableView>,
        synthetic: Arc<Vec<u8>>,
        parent: Option<&str>,
    ) -> Value {
        let items: Vec<Value> = views
            .into_iter()
            .map(|view| {
                let expression = child_expression(parent, &view.name);
                let reference = self.child_reference(&view, &synthetic, expression.clone());
                variable_json(view, reference, expression)
            })
            .collect();
        json!({"variables": items})
    }

    fn variables(&mut self, arguments: &Value) -> Handled {
        let reference = arguments["variablesReference"].as_i64().unwrap_or(0);
        let node = usize::try_from(reference - 1)
            .ok()
            .and_then(|index| self.variables.get(index))
            .ok_or("unknown variable reference")?;
        match *node {
            VarNode::Locals { thread, frame } => {
                let session = self.session()?;
                let views = session.locals(thread, frame).map_err(fail)?;
                let synthetic = Arc::new(session.synthetic_for(thread, frame).map_err(fail)?);
                Ok(self.views_json(views, synthetic, None))
            }
            VarNode::Registers { thread, frame } => {
                let registers = self
                    .session()?
                    .registers_view(thread, frame)
                    .map_err(fail)?;
                let items: Vec<Value> = registers
                    .into_iter()
                    .map(|(name, value)| {
                        json!({
                            "name": name,
                            "value": format!("{value:#010x}"),
                            "variablesReference": 0,
                        })
                    })
                    .collect();
                Ok(json!({"variables": items}))
            }
            VarNode::Globals => {
                let session = self.session()?;
                let globals = session.globals();
                let views: Vec<VariableView> = globals
                    .into_iter()
                    .map(|(name, place)| session.view_global(name, place))
                    .collect();
                Ok(self.views_json(views, Arc::new(Vec::new()), None))
            }
            VarNode::Children {
                place,
                ref synthetic,
                ref expr,
            } => {
                let synthetic = synthetic.clone();
                let expr = expr.clone();
                let views = self
                    .session()?
                    .expand(place, synthetic.as_ref().clone())
                    .map_err(fail)?;
                Ok(self.views_json(views, synthetic, expr.as_deref()))
            }
        }
    }

    fn evaluate(&mut self, arguments: &Value) -> Handled {
        let expression = arguments["expression"]
            .as_str()
            .ok_or("evaluate needs an expression")?;
        let frame = self.frame_of(arguments["frameId"].as_i64());
        let session = self.session()?;
        let view = session.evaluate(expression, frame).map_err(fail)?;
        let synthetic = match frame {
            Some((thread, index)) if *session.state() == RunState::Halted => {
                session.synthetic_for(thread, index).map_err(fail)?
            }
            _ => Vec::new(),
        };
        let reference =
            self.child_reference(&view, &Arc::new(synthetic), Some(expression.to_owned()));
        Ok(json!({
            "result": view.value,
            "type": view.type_name,
            "variablesReference": reference,
        }))
    }

    fn resume(&mut self) -> Handled {
        self.forget_handles();
        self.session()?.resume().map_err(fail)?;
        self.note_running();
        Ok(json!({"allThreadsContinued": true}))
    }

    fn step(&mut self, request: &Value, kind: StepKind) -> bool {
        // Long steps must not hold up the response.
        self.client.respond(request, Ok(json!({})));
        self.forget_handles();
        match self.session().and_then(|s| s.step(kind).map_err(fail)) {
            Ok(reason) => self.report_stop(&reason),
            Err(error) => {
                self.client.console(&format!("step failed: {error}"));
                self.report_stop(&StopReason::Pause);
            }
        }
        true
    }

    fn pause(&mut self) -> Handled {
        // Already halted means a long step was cancelled and reported.
        if self.is_running() {
            let reason = self.session()?.pause().map_err(fail)?;
            self.report_stop(&reason);
        }
        Ok(json!({}))
    }

    fn reload(&mut self, arguments: &Value) -> Handled {
        self.forget_handles();
        // `restart` nests the launch arguments; the custom request is flat.
        let elf = arguments["elf"]
            .as_str()
            .or_else(|| arguments["arguments"]["elf"].as_str())
            .map(PathBuf::from);
        let progress = self.progress();
        // HSS must not be running while flash is being rewritten.
        self.sampler.unbind();
        self.note_halted(&StopReason::Pause);
        let reloaded = self.session()?.reload(elf, progress);
        let resolved = match reloaded {
            Ok(resolved) => resolved,
            Err(error) => {
                self.rebind_sampler();
                return Err(fail(error));
            }
        };
        for breakpoint in &resolved {
            self.client.event(
                "breakpoint",
                json!({"reason": "changed", "breakpoint": describe_breakpoint(breakpoint)}),
            );
        }
        self.client.event("jlinkd/reloaded", json!({}));
        let mode = if self.run_to_main_on_reload {
            AfterConfiguration::RunToMain
        } else {
            AfterConfiguration::Run
        };
        self.apply_start(mode)?;
        self.sampler.note_event(EventKind::Reload, json!({}));
        self.rebind_sampler();
        Ok(json!({}))
    }

    /// Run-control requests from the data channel.
    pub fn control(&mut self, command: Control) -> Handled {
        match command {
            Control::Halt => self.pause(),
            Control::Go => {
                let result = self.resume()?;
                self.client.event(
                    "continued",
                    json!({"threadId": MAIN_THREAD, "allThreadsContinued": true}),
                );
                Ok(result)
            }
            Control::Reset => {
                self.forget_handles();
                self.session()?.reset_halt().map_err(fail)?;
                self.sampler.note_event(EventKind::Reset, json!({}));
                self.apply_start(AfterConfiguration::Run)?;
                self.client.event(
                    "continued",
                    json!({"threadId": MAIN_THREAD, "allThreadsContinued": true}),
                );
                Ok(json!({}))
            }
            Control::Reload { elf } => {
                let arguments = elf.map_or_else(|| json!({}), |elf| json!({"elf": elf}));
                self.reload(&arguments)
            }
        }
    }

    /// End debugging: sampling stops and no breakpoint remains. The probe stays
    /// open for the next session, and the target keeps running unless asked.
    fn finish_debugging(&mut self, halt: bool) -> Handled {
        if std::mem::replace(&mut self.finished, true) {
            return Ok(json!({}));
        }
        self.sampler.unbind();
        if let Some(session) = self.session.as_mut() {
            session.end_debugging(halt).map_err(fail)?;
        }
        Ok(json!({}))
    }

    /// The client vanished: leave the target running with no breakpoints.
    pub fn abandon(&mut self) {
        self.cancel.store(false, Ordering::SeqCst);
        if let Err(error) = self.finish_debugging(false) {
            tracing::warn!("cannot clean up after a lost client: {error}");
        }
    }

    pub fn into_session(self) -> Option<Session> {
        self.session
    }
}
