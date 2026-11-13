//! Debug Adapter Protocol request handling (DESIGN §6.1).

use std::collections::HashSet;
use std::io::{BufWriter, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use engine::breakpoints::{DataRequest, FunctionBreakpoint, LineBreakpoint, ResolvedBreakpoint};
use engine::conditions::BreakpointOptions;
use engine::project::ProjectConfig;
use engine::session::{
    CycleDelta, DisplayFrame, FaultReport, LaunchOptions, RunState, Session, StopReason,
    VariableView, MAIN_THREAD,
};
use engine::step::StepKind;
use probe_api::{ConnectCfg, Interface, Probe, Progress, WpAccess, DEFAULT_SPEED_KHZ};
use probe_jlink::actor::Priority;
use probe_jlink::{JLink, JLinkOptions};
use sampler::codec::joined;
use sampler::{Binding, Sampler, SamplingConfig};
use serde_json::{json, Value};
use symbols::{Place, Scalar, ScalarKind};
use tsdb::EventKind;

use crate::control::Control;
use crate::protocol::write_message;

type Handled = Result<Value, String>;

/// Largest single memory request, to keep the event loop responsive.
const MAX_MEMORY_REQUEST: usize = 1 << 20;
const NOT_READ: &str = "<not read: reading changes it>";
const WRITE_ONLY: &str = "<write-only>";
const UNREADABLE: &str = "<unreadable>";
const IMPRECISE_NOTE: &str = "The reported position is imprecise: the instruction that caused \
                              the error may be a few instructions earlier.";

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
    Fpu {
        thread: i64,
        frame: usize,
    },
    Globals,
    Children {
        place: Place,
        synthetic: Arc<Vec<u8>>,
        expr: Option<String>,
    },
    Peripherals,
    Peripheral(String),
    PeripheralRegister {
        peripheral: String,
        register: String,
        value: u32,
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
    /// The fault of the current stop, as sent to the client.
    fault: Option<Value>,
    fault_thread: Option<i64>,
    last_tasks: Vec<Value>,
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

fn stop_body(reason: &StopReason, thread: i64, fault: Option<&Value>) -> Value {
    let (name, breakpoints) = match reason {
        StopReason::Breakpoint(ids) => ("breakpoint", ids.clone()),
        StopReason::Step | StopReason::StillRunning => ("step", Vec::new()),
        StopReason::Pause | StopReason::External => ("pause", Vec::new()),
        StopReason::Entry => ("entry", Vec::new()),
        StopReason::Data(ids) => ("data breakpoint", ids.clone()),
        StopReason::Fault => ("exception", Vec::new()),
    };
    let mut body = json!({
        "reason": name,
        "threadId": thread,
        "allThreadsStopped": true,
    });
    if !breakpoints.is_empty() {
        body["hitBreakpointIds"] = json!(breakpoints);
    }
    if let Some(fault) = fault {
        body["description"] = fault["summary"].clone();
        body["text"] = fault["name"].clone();
    }
    body
}

fn hex(value: u32) -> String {
    format!("{value:#010x}")
}

fn fault_json(report: &FaultReport) -> Value {
    let registers = &report.registers;
    let causes: Vec<Value> = report
        .causes
        .iter()
        .map(|cause| json!({"register": cause.register, "name": cause.name, "meaning": cause.meaning}))
        .collect();
    let mut out = json!({
        "exception": report.exception,
        "name": report.exception_name,
        "summary": report.summary,
        "causes": causes,
        "imprecise": report.imprecise,
        "cfsr": hex(registers.cfsr),
        "hfsr": hex(registers.hfsr),
        "mmfar": hex(registers.mmfar),
        "bfar": hex(registers.bfar),
        "afsr": hex(registers.afsr),
        "thread": report.thread,
    });
    if report.imprecise {
        out["note"] = json!(IMPRECISE_NOTE);
    }
    if let Some((register, value)) = report.address {
        out["address"] = json!({"register": register, "value": hex(value)});
    }
    if let Some(pc) = report.pc {
        out["pc"] = json!(hex(pc));
    }
    if let Some(function) = &report.function {
        out["function"] = json!(function);
    }
    if let Some(location) = &report.location {
        out["file"] = json!(location.file);
        out["line"] = json!(location.line);
    }
    out
}

fn cycles_json(delta: &CycleDelta) -> Value {
    json!({
        "cycles": delta.cycles,
        "coreHz": delta.core_hz,
        "microseconds": delta.microseconds,
        "wrapped": delta.wrapped,
    })
}

fn breakpoint_options(item: &Value) -> BreakpointOptions {
    let text = |key: &str| item[key].as_str().map(str::to_owned);
    BreakpointOptions {
        condition: text("condition"),
        hit_condition: text("hitCondition"),
        log_message: text("logMessage"),
    }
}

/// Encodes what a data breakpoint watches as `address:size:expression`.
fn data_id(address: u32, size: u32, expression: Option<&str>) -> String {
    format!("{address:#x}:{size}:{}", expression.unwrap_or(""))
}

fn parse_data_id(id: &str) -> Option<(u32, u32, Option<String>)> {
    let mut parts = id.splitn(3, ':');
    let address = u32::from_str_radix(parts.next()?.strip_prefix("0x")?, 16).ok()?;
    let size = parts.next()?.parse().ok()?;
    let expression = parts.next().filter(|e| !e.is_empty()).map(str::to_owned);
    Some((address, size, expression))
}

fn parse_access(text: Option<&str>) -> WpAccess {
    match text {
        Some("read") => WpAccess::Read,
        Some("readWrite") => WpAccess::ReadWrite,
        _ => WpAccess::Write,
    }
}

fn parse_address(reference: &str, offset: i64) -> Result<u32, String> {
    let base = match reference.strip_prefix("0x") {
        Some(digits) => u32::from_str_radix(digits, 16),
        None => reference.parse(),
    }
    .map_err(|_| format!("invalid memory reference {reference:?}"))?;
    u32::try_from(i64::from(base) + offset)
        .map_err(|_| "the address is outside the address space".to_owned())
}

fn parse_word(text: &str) -> Result<u32, String> {
    let scalar = Scalar {
        kind: ScalarKind::Unsigned,
        size: 4,
        bits: None,
    };
    scalar.encode(text).map(|value| value as u32)
}

fn register_text(value: u32, bits: u32) -> String {
    format!("{value:#0width$x}", width = (bits as usize / 4) + 2)
}

/// A register's value, unless reading it would be harmful. `force` is the
/// user asking for this one register anyway.
fn read_svd_register(
    session: &Session,
    register: &svd::Register,
    force: bool,
) -> (Option<u32>, String) {
    if register.access == svd::Access::WriteOnly {
        return (None, WRITE_ONLY.to_owned());
    }
    if register.read_has_side_effects && !force {
        return (None, NOT_READ.to_owned());
    }
    match session.read_peripheral_register(register) {
        Ok(value) => (Some(value), register_text(value, register.bits)),
        Err(_) => (None, UNREADABLE.to_owned()),
    }
}

fn field_value_text(field: &svd::Field, register: u32) -> String {
    let value = field.extract(register);
    match field.value_name(value) {
        Some(name) => format!("{value} ({name})"),
        None => format!("{value}"),
    }
}

fn field_bits(field: &svd::Field) -> String {
    if field.width == 1 {
        format!("[{}]", field.lsb)
    } else {
        format!("[{}:{}]", field.lsb + field.width - 1, field.lsb)
    }
}

/// A value for a field: a number, or one of the field's declared names.
fn field_value(field: &svd::Field, text: &str) -> Result<u32, String> {
    let named = field
        .values
        .iter()
        .find(|(_, name)| name.eq_ignore_ascii_case(text.trim()))
        .map(|(value, _)| *value as u32);
    match named {
        Some(value) => Ok(value),
        None => parse_word(text),
    }
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

/// An empty value counts as not configured, as `${env:X}` can expand to one.
fn parse_serial(arguments: &Value) -> Result<Option<u32>, String> {
    match &arguments["serial"] {
        Value::Null => Ok(None),
        Value::String(text) if text.is_empty() => Ok(None),
        Value::String(text) => text
            .parse()
            .map(Some)
            .map_err(|_| format!("invalid serial {text:?}")),
        Value::Number(number) => number
            .as_u64()
            .and_then(|value| u32::try_from(value).ok())
            .map(Some)
            .ok_or_else(|| format!("invalid serial {number}")),
        other => Err(format!("invalid serial {other}")),
    }
}

fn launch_options(arguments: &Value) -> Result<LaunchOptions, String> {
    let elf = arguments["elf"]
        .as_str()
        .ok_or("the launch configuration needs \"elf\"")?;
    let mut project = match arguments["project"].as_str() {
        Some(path) if std::path::Path::new(path).exists() => {
            ProjectConfig::load(std::path::Path::new(path))?
        }
        _ => ProjectConfig::default(),
    };
    if let Some(svd) = arguments["svd"].as_str() {
        project.svd = Some(svd.to_owned());
    }
    Ok(LaunchOptions {
        connect: ConnectCfg {
            device: arguments["device"]
                .as_str()
                .ok_or("the launch configuration needs \"device\"")?
                .to_owned(),
            interface: parse_interface(arguments)?,
            speed_khz: parse_speed(arguments)?,
        },
        probe: JLinkOptions {
            dll: arguments["dll"]
                .as_str()
                .filter(|path| !path.is_empty())
                .map(PathBuf::from),
            serial: parse_serial(arguments)?,
            log_file: None,
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
    let place = view.place;
    let mut out = json!({
        "name": view.name,
        "value": view.value,
        "type": view.type_name,
        "variablesReference": reference,
    });
    if let Some(expression) = expression {
        out["evaluateName"] = json!(expression);
    }
    if let Some(place) = place.filter(Session::place_in_memory) {
        out["memoryReference"] = json!(hex(place.addr));
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
            fault: None,
            fault_thread: None,
            last_tasks: Vec::new(),
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
        self.fault = None;
        self.fault_thread = None;
    }

    /// Show what breakpoints have logged.
    fn flush_output(&mut self) {
        let Some(session) = self.session.as_mut() else {
            return;
        };
        for line in session.take_output() {
            self.client.console(&line);
        }
    }

    /// Work out the reason for a fault and tell the client.
    fn capture_fault(&mut self, thread: i64) {
        let Some(session) = self.session.as_mut() else {
            return;
        };
        match session.fault_report() {
            Ok(report) => {
                let fault = fault_json(&report);
                let place = match &report.location {
                    Some(location) => format!(" ({}:{})", location.file, location.line),
                    None => String::new(),
                };
                self.client
                    .console(&format!("Fault: {}{place}", report.summary));
                if report.imprecise {
                    self.client.console(IMPRECISE_NOTE);
                }
                self.client.event("jlinkd/fault", fault.clone());
                self.fault = Some(fault);
                self.fault_thread = Some(thread);
            }
            Err(error) => self
                .client
                .console(&format!("cannot analyse the fault: {error}")),
        }
    }

    fn report_cycles(&mut self) {
        let Some(session) = self.session.as_mut() else {
            return;
        };
        match session.note_cycles() {
            Ok(Some(delta)) => self.client.event("jlinkd/cycles", cycles_json(&delta)),
            Ok(None) => {}
            Err(error) => tracing::debug!("cannot read the cycle counter: {error}"),
        }
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
            let detail = self
                .fault
                .as_ref()
                .map_or_else(|| json!({}), |fault| json!({"summary": fault["summary"]}));
            self.sampler.note_event(EventKind::Fault, detail);
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
        self.flush_output();
        let thread = self.current_thread();
        if matches!(reason, StopReason::Fault) {
            self.capture_fault(thread);
        }
        self.note_halted(reason);
        self.sampler.note_stopped();
        self.report_cycles();
        self.client
            .event("stopped", stop_body(reason, thread, self.fault.as_ref()));
    }

    /// Check for a stop while the target runs.
    pub fn poll(&mut self) {
        let Some(session) = self.session.as_mut() else {
            return;
        };
        match session.poll() {
            Ok(Some(reason)) => self.report_stop(&reason),
            Ok(None) => self.flush_output(),
            Err(error) => {
                tracing::warn!("polling the target failed: {error}");
                self.client.console(&format!("probe error: {error}"));
                if let Some(session) = self.session.as_mut() {
                    session.mark_halted();
                }
            }
        }
    }

    /// How long the event loop may wait before polling the target again.
    pub fn poll_wait(&self, idle: Duration) -> Duration {
        let busy =
            self.is_running() && self.session.as_ref().is_some_and(Session::is_auto_resuming);
        if busy {
            Duration::ZERO
        } else {
            idle
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
            "setExceptionBreakpoints" => self.set_exception_breakpoints(arguments),
            "dataBreakpointInfo" => self.data_breakpoint_info(arguments),
            "setDataBreakpoints" => self.set_data_breakpoints(arguments),
            "exceptionInfo" => self.exception_info(),
            "readMemory" => self.read_memory(arguments),
            "writeMemory" => self.write_memory(arguments),
            "setVariable" => self.set_variable(arguments),
            "jlinkd/faultInfo" => self.fault_info(),
            "jlinkd/rtosTasks" => self.rtos_tasks(),
            "jlinkd/cycleCounter" => self.cycle_counter(),
            "jlinkd/peripherals" => self.peripheral_list(),
            "jlinkd/peripheralRead" => self.peripheral_read(arguments),
            "jlinkd/peripheralWrite" => self.peripheral_write(arguments),
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
            "supportsConditionalBreakpoints": true,
            "supportsHitConditionalBreakpoints": true,
            "supportsLogPoints": true,
            "supportsDataBreakpoints": true,
            "supportsReadMemoryRequest": true,
            "supportsWriteMemoryRequest": true,
            "supportsSetVariable": true,
            "supportsExceptionInfoRequest": true,
            "exceptionBreakpointFilters": [
                {"filter": "hardfault", "label": "HardFault", "default": true},
                {"filter": "busfault", "label": "BusFault", "default": true},
                {"filter": "memmanage", "label": "MemManage", "default": true},
                {"filter": "usagefault", "label": "UsageFault", "default": true},
            ],
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
        let probe = JLink::open(&options.probe).map_err(fail)?;
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
        let lines: Vec<LineBreakpoint> = arguments["breakpoints"]
            .as_array()
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| {
                        Some(LineBreakpoint {
                            line: item["line"].as_u64()? as u32,
                            options: breakpoint_options(item),
                        })
                    })
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
        let functions: Vec<FunctionBreakpoint> = arguments["breakpoints"]
            .as_array()
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| {
                        Some(FunctionBreakpoint {
                            name: item["name"].as_str()?.to_owned(),
                            options: breakpoint_options(item),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        let resolved = self
            .session()?
            .set_function_breakpoints(&functions)
            .map_err(fail)?;
        let items: Vec<Value> = resolved.iter().map(describe_breakpoint).collect();
        Ok(json!({"breakpoints": items}))
    }

    fn set_exception_breakpoints(&mut self, arguments: &Value) -> Handled {
        let filters: Vec<String> = arguments["filters"]
            .as_array()
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| item.as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default();
        self.session()?
            .set_exception_filters(&filters)
            .map_err(fail)?;
        Ok(json!({}))
    }

    /// The object behind an entry of the Variables view.
    fn place_of(
        &mut self,
        reference: i64,
        name: &str,
    ) -> Result<(Option<Place>, Option<String>), String> {
        let index = usize::try_from(reference - 1).map_err(|_| "unknown variable reference")?;
        let node = self
            .variables
            .get(index)
            .ok_or("unknown variable reference")?;
        match *node {
            VarNode::Locals { thread, frame } => {
                let place = self
                    .session()?
                    .local_place(thread, frame, name)
                    .map_err(fail)?;
                // A local has no name that means anything after a reload.
                Ok((place, None))
            }
            VarNode::Globals => {
                let session = self.session()?;
                let place = session
                    .globals()
                    .into_iter()
                    .find(|(global, _)| global == name)
                    .map(|(_, place)| place);
                Ok((place, Some(name.to_owned())))
            }
            VarNode::Children {
                place,
                ref synthetic,
                ref expr,
            } => {
                let (synthetic, expr) = (synthetic.clone(), expr.clone());
                let views = self
                    .session()?
                    .expand(place, synthetic.as_ref().clone())
                    .map_err(fail)?;
                let child = views.into_iter().find(|view| view.name == name);
                Ok((
                    child.and_then(|view| view.place),
                    child_expression(expr.as_deref(), name),
                ))
            }
            _ => Ok((None, None)),
        }
    }

    fn data_breakpoint_info(&mut self, arguments: &Value) -> Handled {
        let name = arguments["name"]
            .as_str()
            .ok_or("dataBreakpointInfo needs a name")?;
        let reference = arguments["variablesReference"].as_i64().unwrap_or(0);
        let (place, expression) = if reference > 0 {
            self.place_of(reference, name)?
        } else {
            let frame = self.frame_of(arguments["frameId"].as_i64());
            let view = self.session()?.evaluate(name, frame).map_err(fail)?;
            (view.place, Some(name.to_owned()))
        };
        let refuse = |why: &str| Ok(json!({"dataId": null, "description": why}));
        let Some(place) = place.filter(Session::place_in_memory) else {
            return refuse("this value is not in memory");
        };
        if place.bits.is_some() {
            return refuse("a bit-field cannot be watched by itself");
        }
        let size = self.session()?.place_size(&place).map_err(fail)?;
        if !size.is_power_of_two() || !place.addr.is_multiple_of(size) {
            return refuse("the hardware can only watch a power-of-two size aligned to itself");
        }
        Ok(json!({
            "dataId": data_id(place.addr, size, expression.as_deref()),
            "description": format!("{name} ({size} bytes at {})", hex(place.addr)),
            "accessTypes": ["read", "write", "readWrite"],
            "canPersist": false,
        }))
    }

    fn set_data_breakpoints(&mut self, arguments: &Value) -> Handled {
        let mut requests = Vec::new();
        for item in arguments["breakpoints"].as_array().into_iter().flatten() {
            let id = item["dataId"]
                .as_str()
                .ok_or("a data breakpoint needs a dataId")?;
            let (addr, size, expression) =
                parse_data_id(id).ok_or_else(|| format!("invalid dataId {id:?}"))?;
            requests.push(DataRequest {
                addr,
                size,
                access: parse_access(item["accessType"].as_str()),
                expression,
                options: breakpoint_options(item),
            });
        }
        let resolved = self
            .session()?
            .set_data_breakpoints(&requests)
            .map_err(fail)?;
        let items: Vec<Value> = resolved.iter().map(describe_breakpoint).collect();
        Ok(json!({"breakpoints": items}))
    }

    fn exception_info(&mut self) -> Handled {
        let fault = self.fault.as_ref().ok_or("no fault at this stop")?;
        let mut message = fault["summary"].as_str().unwrap_or("").to_owned();
        if let Some(note) = fault["note"].as_str() {
            message.push('\n');
            message.push_str(note);
        }
        Ok(json!({
            "exceptionId": fault["name"],
            "description": message,
            "breakMode": "always",
            "details": {
                "message": fault["summary"],
                "typeName": fault["name"],
                "evaluateName": fault["function"],
            },
        }))
    }

    fn fault_info(&mut self) -> Handled {
        if self.fault.is_none() && !self.is_running() {
            let report = self.session()?.fault_report().map_err(fail)?;
            return Ok(fault_json(&report));
        }
        self.fault
            .clone()
            .ok_or_else(|| "the target is running".to_owned())
    }

    fn cycle_counter(&mut self) -> Handled {
        Ok(self
            .session()?
            .last_cycles()
            .map_or(Value::Null, |delta| cycles_json(&delta)))
    }

    fn rtos_tasks(&mut self) -> Handled {
        if self.is_running() {
            return Ok(json!({"tasks": self.last_tasks, "stale": true}));
        }
        let Some(tasks) = self.session()?.tasks().map_err(fail)? else {
            return Ok(json!({"tasks": []}));
        };
        self.last_tasks = tasks
            .iter()
            .map(|task| {
                json!({
                    "id": task.tcb,
                    "name": task.name,
                    "state": task.state.name(),
                    "priority": task.priority,
                    "basePriority": task.base_priority,
                    "stackBase": hex(task.stack_base),
                    "topOfStack": hex(task.top_of_stack),
                    "stackFreeBytes": task.stack_free_bytes,
                })
            })
            .collect();
        Ok(json!({"tasks": self.last_tasks}))
    }

    fn read_memory(&mut self, arguments: &Value) -> Handled {
        let reference = arguments["memoryReference"]
            .as_str()
            .ok_or("readMemory needs a memoryReference")?;
        let address = parse_address(reference, arguments["offset"].as_i64().unwrap_or(0))?;
        let count = arguments["count"].as_u64().unwrap_or(0) as usize;
        if count > MAX_MEMORY_REQUEST {
            return Err(format!("reading {count} bytes at once is not supported"));
        }
        let bytes = self.session()?.read_memory(address, count).map_err(fail)?;
        let mut body = json!({"address": hex(address)});
        if count > bytes.len() {
            body["unreadableBytes"] = json!(count - bytes.len());
        }
        if !bytes.is_empty() {
            body["data"] = json!(BASE64.encode(&bytes));
        }
        Ok(body)
    }

    fn write_memory(&mut self, arguments: &Value) -> Handled {
        let reference = arguments["memoryReference"]
            .as_str()
            .ok_or("writeMemory needs a memoryReference")?;
        let address = parse_address(reference, arguments["offset"].as_i64().unwrap_or(0))?;
        let data = BASE64
            .decode(arguments["data"].as_str().unwrap_or(""))
            .map_err(|_| "the data is not valid base64".to_owned())?;
        let length = data.len();
        self.session()?.write_memory(address, data).map_err(fail)?;
        self.client
            .event("invalidated", json!({"areas": ["variables"]}));
        Ok(json!({"bytesWritten": length}))
    }

    fn set_variable(&mut self, arguments: &Value) -> Handled {
        let reference = arguments["variablesReference"].as_i64().unwrap_or(0);
        let name = arguments["name"]
            .as_str()
            .ok_or("setVariable needs a name")?;
        let value = arguments["value"]
            .as_str()
            .ok_or("setVariable needs a value")?;
        let index = usize::try_from(reference - 1).map_err(|_| "unknown variable reference")?;
        let node = self
            .variables
            .get(index)
            .ok_or("unknown variable reference")?;
        match *node {
            VarNode::Registers { thread, frame } | VarNode::Fpu { thread, frame } => {
                let written = self
                    .session()?
                    .write_register(thread, frame, name, value)
                    .map_err(fail)?;
                Ok(json!({"value": hex(written)}))
            }
            VarNode::Peripheral(ref peripheral) => {
                let peripheral = peripheral.clone();
                self.write_peripheral(&peripheral, name, None, value)
            }
            VarNode::PeripheralRegister {
                ref peripheral,
                ref register,
                ..
            } => {
                let (peripheral, register) = (peripheral.clone(), register.clone());
                self.write_peripheral(&peripheral, &register, Some(name), value)
            }
            _ => {
                let (place, _) = self.place_of(reference, name)?;
                let place = place.ok_or("this value cannot be changed")?;
                let session = self.session()?;
                session.write_place(place, value).map_err(fail)?;
                let view = session.view_global(name.to_owned(), place);
                Ok(json!({"value": view.value}))
            }
        }
    }

    fn device(&mut self) -> Result<Arc<svd::Device>, String> {
        self.session()?
            .peripherals()
            .map_err(fail)?
            .ok_or_else(|| "no SVD file: set \"svd\" in launch.json".to_owned())
    }

    fn write_peripheral(
        &mut self,
        peripheral: &str,
        register: &str,
        field: Option<&str>,
        text: &str,
    ) -> Handled {
        let device = self.device()?;
        let target = device
            .peripheral(peripheral)
            .and_then(|found| found.register(register))
            .ok_or_else(|| format!("no register {peripheral}.{register}"))?;
        if !target.access.can_write() {
            return Err(format!("{register} is read-only"));
        }
        let session = self.session()?;
        let Some(field_name) = field else {
            let value = parse_word(text)?;
            session
                .write_peripheral_register(target, value)
                .map_err(fail)?;
            return Ok(json!({"value": register_text(value, target.bits)}));
        };
        let found = target
            .fields
            .iter()
            .find(|candidate| candidate.name == field_name)
            .ok_or_else(|| format!("no field {field_name}"))?;
        if !found.access.can_write() {
            return Err(format!("{field_name} is read-only"));
        }
        // A write-only register reads as nothing useful, so start from zero.
        let current = if target.access == svd::Access::WriteOnly {
            0
        } else {
            session.read_peripheral_register(target).map_err(fail)?
        };
        let updated = found
            .insert(current, field_value(found, text)?)
            .ok_or_else(|| format!("{text} does not fit in {} bits", found.width))?;
        session
            .write_peripheral_register(target, updated)
            .map_err(fail)?;
        Ok(json!({"value": field_value_text(found, updated)}))
    }

    fn peripheral_list(&mut self) -> Handled {
        let Some(device) = self.session()?.peripherals().map_err(fail)? else {
            return Ok(json!({"peripherals": []}));
        };
        let items: Vec<Value> = device
            .peripherals
            .iter()
            .map(|peripheral| {
                json!({
                    "name": peripheral.name,
                    "description": peripheral.description,
                    "base": hex(peripheral.base),
                })
            })
            .collect();
        Ok(json!({"device": device.name, "peripherals": items}))
    }

    /// Current values of a peripheral's registers. `force` names one register
    /// to read even though reading it has side effects.
    fn peripheral_read(&mut self, arguments: &Value) -> Handled {
        let name = arguments["peripheral"]
            .as_str()
            .ok_or("peripheralRead needs a peripheral")?;
        let force = arguments["force"].as_str();
        let device = self.device()?;
        let peripheral = device
            .peripheral(name)
            .ok_or_else(|| format!("no peripheral {name}"))?;
        let session = self.session()?;
        let registers: Vec<Value> = peripheral
            .registers
            .iter()
            .map(|register| {
                let forced = force == Some(register.name.as_str());
                let (value, text) = read_svd_register(session, register, forced);
                let fields: Vec<Value> = register
                    .fields
                    .iter()
                    .map(|field| {
                        json!({
                            "name": field.name,
                            "description": field.description,
                            "bits": field_bits(field),
                            "value": value.map(|value| field_value_text(field, value)),
                            "writable": field.access.can_write() && register.access.can_write(),
                        })
                    })
                    .collect();
                json!({
                    "name": register.name,
                    "description": register.description,
                    "address": hex(register.address),
                    "value": text,
                    "known": value.is_some(),
                    "sideEffects": register.read_has_side_effects,
                    "writable": register.access.can_write(),
                    "fields": fields,
                })
            })
            .collect();
        Ok(json!({"registers": registers}))
    }

    fn peripheral_write(&mut self, arguments: &Value) -> Handled {
        let peripheral = arguments["peripheral"]
            .as_str()
            .ok_or("needs a peripheral")?;
        let register = arguments["register"].as_str().ok_or("needs a register")?;
        let value = arguments["value"].as_str().ok_or("needs a value")?;
        self.write_peripheral(peripheral, register, arguments["field"].as_str(), value)
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
        // At a fault, the editor should open on the code that faulted rather
        // than the handler. It skips frames whose source is deemphasized.
        let handler_frames = match self.fault_thread {
            Some(faulted) if faulted == thread => frames
                .iter()
                .position(|frame| frame.is_exception)
                .unwrap_or(0),
            _ => 0,
        };
        let items: Vec<Value> = frames
            .iter()
            .enumerate()
            .skip(start)
            .take(levels)
            .map(|(index, frame)| {
                let mut item = self.frame_json(thread, index, frame);
                if index < handler_frames && item["source"].is_object() {
                    item["source"]["presentationHint"] = json!("deemphasize");
                }
                item
            })
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
        let mut scopes = vec![
            json!({"name": "Locals", "presentationHint": "locals", "variablesReference": locals, "expensive": false}),
            json!({"name": "Registers", "presentationHint": "registers", "variablesReference": registers, "expensive": false}),
            json!({"name": "Globals", "variablesReference": globals, "expensive": true}),
        ];
        // A bad SVD file is reported when the user opens the view, not here.
        if !matches!(self.session()?.peripherals(), Ok(None)) {
            let peripherals = self.add_var(VarNode::Peripherals);
            scopes.push(json!({"name": "Peripherals", "variablesReference": peripherals, "expensive": true}));
        }
        Ok(json!({"scopes": scopes}))
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
                let mut items: Vec<Value> = registers
                    .into_iter()
                    .map(|(name, value)| {
                        json!({
                            "name": name,
                            "value": hex(value),
                            "variablesReference": 0,
                        })
                    })
                    .collect();
                if self
                    .session()?
                    .fpu_view(thread, frame)
                    .map_err(fail)?
                    .is_some()
                {
                    let reference = self.add_var(VarNode::Fpu { thread, frame });
                    items
                        .push(json!({"name": "FPU", "value": "", "variablesReference": reference}));
                }
                Ok(json!({"variables": items}))
            }
            VarNode::Fpu { thread, frame } => {
                let (singles, status) = self
                    .session()?
                    .fpu_view(thread, frame)
                    .map_err(fail)?
                    .ok_or("the FPU is not available in this frame")?;
                let mut items =
                    vec![json!({"name": "fpscr", "value": hex(status), "variablesReference": 0})];
                items.extend(singles.iter().enumerate().map(|(index, bits)| {
                    json!({
                        "name": format!("s{index}"),
                        "value": format!("{} ({})", f32::from_bits(*bits), hex(*bits)),
                        "variablesReference": 0,
                    })
                }));
                items.extend(
                    singles
                        .as_chunks::<2>()
                        .0
                        .iter()
                        .enumerate()
                        .map(|(index, pair)| {
                            let bits = (u64::from(pair[1]) << 32) | u64::from(pair[0]);
                            json!({
                                "name": format!("d{index}"),
                                "value": format!("{} ({bits:#018x})", f64::from_bits(bits)),
                                "variablesReference": 0,
                            })
                        }),
                );
                Ok(json!({"variables": items}))
            }
            VarNode::Peripherals => {
                let device = self.device()?;
                let items: Vec<Value> = device
                    .peripherals
                    .iter()
                    .map(|peripheral| {
                        let reference = self.add_var(VarNode::Peripheral(peripheral.name.clone()));
                        json!({
                            "name": peripheral.name,
                            "value": hex(peripheral.base),
                            "type": peripheral.description,
                            "variablesReference": reference,
                        })
                    })
                    .collect();
                Ok(json!({"variables": items}))
            }
            VarNode::Peripheral(ref name) => {
                let name = name.clone();
                let device = self.device()?;
                let peripheral = device
                    .peripheral(&name)
                    .ok_or_else(|| format!("no peripheral {name}"))?;
                let mut items = Vec::with_capacity(peripheral.registers.len());
                for register in &peripheral.registers {
                    let (value, text) = read_svd_register(self.session()?, register, false);
                    let reference = match value {
                        Some(value) if !register.fields.is_empty() => {
                            self.add_var(VarNode::PeripheralRegister {
                                peripheral: name.clone(),
                                register: register.name.clone(),
                                value,
                            })
                        }
                        _ => 0,
                    };
                    items.push(json!({
                        "name": register.name,
                        "value": text,
                        "type": register.description,
                        "memoryReference": hex(register.address),
                        "variablesReference": reference,
                    }));
                }
                Ok(json!({"variables": items}))
            }
            VarNode::PeripheralRegister {
                ref peripheral,
                ref register,
                value,
            } => {
                let (peripheral, register) = (peripheral.clone(), register.clone());
                let device = self.device()?;
                let target = device
                    .peripheral(&peripheral)
                    .and_then(|found| found.register(&register))
                    .ok_or_else(|| format!("no register {peripheral}.{register}"))?;
                let items: Vec<Value> = target
                    .fields
                    .iter()
                    .map(|field| {
                        json!({
                            "name": field.name,
                            "value": field_value_text(field, value),
                            "type": format!("{} {}", field_bits(field), field.description),
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
        let memory_reference = view
            .place
            .filter(Session::place_in_memory)
            .map(|p| hex(p.addr));
        let reference =
            self.child_reference(&view, &Arc::new(synthetic), Some(expression.to_owned()));
        let mut body = json!({
            "result": view.value,
            "type": view.type_name,
            "variablesReference": reference,
        });
        if let Some(memory) = memory_reference {
            body["memoryReference"] = json!(memory);
        }
        Ok(body)
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
            Ok(StopReason::StillRunning) => {
                self.client.console(
                    "This step has not finished, so the target keeps running. \
                     It stops when the step completes; Pause stops it now.",
                );
                self.note_running();
                self.client.event(
                    "continued",
                    json!({"threadId": MAIN_THREAD, "allThreadsContinued": true}),
                );
            }
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
