//! Debug session: run state, stop detection, stack frames and variables
//! (DESIGN §5.5). All probe access goes through the actor.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use probe_api::{
    apply_post_connect, AccessWidth, BpHandle, BpKind, ConnectCfg, DeviceProfile,
    PostConnectOptions, Probe, ProbeError, ProbeExt, Reg, ResetKind, TargetInfo, VC_ALL_FAULTS,
    VC_BUSFAULT, VC_HARDFAULT, VC_MEMMANAGE, VC_USAGEFAULT,
};
use probe_jlink::actor::{Priority, ProbeActor, ProbeHandle};
use probe_jlink::JLinkOptions;
use symbols::{
    CoreRegisters, FormatOptions, FrameKind, FrameRegisters, LoadOptions, LocalVar, Located,
    MemoryReader, Number, Place, Scalar, ScalarKind, SourceLocation, SymbolDb, SymbolError,
    UnwindStopReason,
};

use crate::breakpoints::{
    BreakpointTable, DataRequest, FunctionBreakpoint, LineBreakpoint, Placement, ResolvedBreakpoint,
};
use crate::conditions::LogPart;
use crate::fault::{self, FaultCause, FaultRegisters};
use crate::project::{family_for_device, DebugConfig, ProjectConfig};
use crate::rtos::freertos::{FreeRtos, Task, TaskState};
use crate::step::{DetachedOutcome, DetachedRun, StepKind};
use visualizer::{Visualized, Visualizers};

const DHCSR: u32 = 0xE000_EDF0;
const DHCSR_KEY: u32 = 0xA05F_0000;
const DHCSR_C_DEBUGEN: u32 = 1 << 0;
const DHCSR_C_HALT: u32 = 1 << 1;
const DHCSR_C_MASKINTS: u32 = 1 << 3;
const DHCSR_S_REGRDY: u32 = 1 << 16;
const DHCSR_S_HALT: u32 = 1 << 17;
const DCRSR: u32 = 0xE000_EDF4;
const DCRDR: u32 = 0xE000_EDF8;
const DFSR: u32 = 0xE000_ED30;
const DFSR_BKPT: u32 = 1 << 1;
const DFSR_DWTTRAP: u32 = 1 << 2;
const DFSR_VCATCH: u32 = 1 << 3;
const DFSR_EXTERNAL: u32 = 1 << 4;
const DFSR_ALL: u32 = 0x1F;
const DEMCR: u32 = 0xE000_EDFC;
const FP_CTRL: u32 = 0xE000_2000;
const FP_COMP0: u32 = 0xE000_2008;
const DWT_CTRL: u32 = 0xE000_1000;
const DWT_CYCCNT: u32 = 0xE000_1004;
const DWT_COMP0: u32 = 0xE000_1020;
const DWT_FUNCTION0: u32 = 0xE000_1028;
const DWT_COMPARATOR_STRIDE: u32 = 16;
const DWT_FUNCTION_MATCHED: u32 = 1 << 24;
const MVFR0: u32 = 0xE000_EF40;
const IPSR_MASK: u32 = 0x1FF;
const CPUID_PART_SHIFT: u32 = 4;
const CPUID_PART_MASK: u32 = 0xFFF;
/// Cortex-M4, M7 and M33, the cores that can have an FPU.
const FPU_CAPABLE_PARTS: [u32; 3] = [0xC24, 0xC27, 0xD21];
/// How long one poll keeps serving breakpoints that resume by themselves.
const AUTO_RESUME_BURST: Duration = Duration::from_millis(15);
const MEMORY_CHUNK: usize = 1024;
const HALT_POLL: Duration = Duration::from_millis(5);
/// Above the Cortex-M memory map's useful range; never read on the target.
const SYNTHETIC_BASE: u32 = 0xFFF0_0000;

/// Largest aggregate summarized on one line.
const SUMMARY_LIMIT: u64 = 4096;
const SUMMARY_OPTIONS: FormatOptions = FormatOptions {
    max_elements: 8,
    max_depth: 2,
};

/// Deepest call stack shown.
const MAX_DISPLAY_FRAMES: usize = 64;

/// Bare-metal images have one thread with this id.
pub const MAIN_THREAD: i64 = 1;

pub type Result<T, E = EngineError> = std::result::Result<T, E>;

#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error(transparent)]
    Probe(#[from] ProbeError),
    #[error(transparent)]
    Symbol(#[from] SymbolError),
    #[error("{0}")]
    Other(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StopReason {
    Breakpoint(Vec<i64>),
    Step,
    Pause,
    Entry,
    /// The ids of the data breakpoints that may have matched.
    Data(Vec<i64>),
    Fault,
    External,
    /// A step is taking a long time. The target runs on and the event loop is
    /// free; [`Session::poll`] reports the stop when the step finishes.
    StillRunning,
}

/// How a wait for the core to halt ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Waited {
    Halted,
    Cancelled,
    TimedOut,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RunState {
    Running,
    Halted,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HaltedPoll {
    Unchanged,
    ResumedExternally,
    MovedExternally(StopReason),
}

#[derive(Clone, Debug)]
pub struct LaunchOptions {
    pub connect: ConnectCfg,
    /// Which J-Link library and probe to open; empty means the defaults.
    pub probe: JLinkOptions,
    pub elf: PathBuf,
    pub project: ProjectConfig,
    /// Base for relative paths in the project file.
    pub workspace: Option<PathBuf>,
}

/// A row of the call stack; several can share one real frame when inlined.
#[derive(Clone, Debug)]
pub struct DisplayFrame {
    pub name: String,
    pub location: Option<SourceLocation>,
    pub pc: u32,
    pub is_inlined: bool,
    pub is_exception: bool,
    pub is_label: bool,
    /// Index into the real frames (only valid when `!is_label`).
    real: usize,
    /// Position in that frame's inline chain.
    chain_index: usize,
}

/// Real frames of the thread most recently asked about.
pub(crate) struct UnwoundStack {
    thread: i64,
    frames: Vec<symbols::StackFrame>,
    display: Vec<DisplayFrame>,
    stop_reason: UnwindStopReason,
}

#[derive(Clone, Debug)]
pub struct VariableView {
    pub name: String,
    pub type_name: String,
    pub value: String,
    pub place: Option<Place>,
    pub expandable: bool,
}

/// Overlay giving register-held values an address, so paths and member access
/// treat them like memory.
pub struct FrameMemory<'a> {
    inner: &'a mut dyn MemoryReader,
    synthetic: Vec<u8>,
}

impl MemoryReader for FrameMemory<'_> {
    fn read(&mut self, addr: u32, buf: &mut [u8]) -> std::result::Result<(), String> {
        if addr >= SYNTHETIC_BASE {
            let start = (addr - SYNTHETIC_BASE) as usize;
            let bytes = self
                .synthetic
                .get(start..start + buf.len())
                .ok_or("outside the value")?;
            buf.copy_from_slice(bytes);
            return Ok(());
        }
        self.inner.read(addr, buf)
    }
}

/// Block cache over the probe, valid while the target stays halted.
pub struct HaltedMemory {
    handle: ProbeHandle,
    priority: Priority,
    blocks: std::collections::HashMap<u32, Option<Box<[u8; BLOCK]>>>,
}

const BLOCK: usize = 64;

impl HaltedMemory {
    pub fn new(handle: ProbeHandle) -> Self {
        Self::with_priority(handle, Priority::P0)
    }

    /// Read at a lower priority, for background work.
    pub fn with_priority(handle: ProbeHandle, priority: Priority) -> Self {
        Self {
            handle,
            priority,
            blocks: Default::default(),
        }
    }

    fn read_direct(&self, addr: u32, buf: &mut [u8]) -> std::result::Result<(), String> {
        let length = buf.len();
        let bytes = self
            .handle
            .call_blocking(self.priority, move |probe: &mut dyn Probe| {
                let mut bytes = vec![0u8; length];
                probe
                    .read_mem(addr, &mut bytes, AccessWidth::Auto)
                    .map(|()| bytes)
            })
            .map_err(|error| error.to_string())?
            .map_err(|error| error.to_string())?;
        buf.copy_from_slice(&bytes);
        Ok(())
    }

    fn block(&mut self, base: u32) -> Option<&[u8; BLOCK]> {
        if !self.blocks.contains_key(&base) {
            let mut bytes = Box::new([0u8; BLOCK]);
            let loaded = self
                .read_direct(base, &mut bytes[..])
                .is_ok()
                .then_some(bytes);
            self.blocks.insert(base, loaded);
        }
        self.blocks[&base].as_deref()
    }

    pub fn clear(&mut self) {
        self.blocks.clear();
    }
}

impl MemoryReader for HaltedMemory {
    fn read(&mut self, addr: u32, buf: &mut [u8]) -> std::result::Result<(), String> {
        if buf.len() > 4 * BLOCK {
            return self.read_direct(addr, buf);
        }
        let mut offset = 0;
        while offset < buf.len() {
            let current = addr.wrapping_add(offset as u32);
            let base = current & !(BLOCK as u32 - 1);
            let within = (current - base) as usize;
            let count = (BLOCK - within).min(buf.len() - offset);
            match self.block(base) {
                Some(block) => {
                    buf[offset..offset + count].copy_from_slice(&block[within..within + count])
                }
                // A block can straddle mapped and unmapped memory.
                None => return self.read_direct(addr, buf),
            }
            offset += count;
        }
        Ok(())
    }
}

/// Why the core entered a fault handler and where it was.
#[derive(Clone, Debug)]
pub struct FaultReport {
    pub exception: u32,
    pub exception_name: String,
    pub summary: String,
    pub registers: FaultRegisters,
    pub causes: Vec<FaultCause>,
    pub address: Option<(&'static str, u32)>,
    pub imprecise: bool,
    pub thread: i64,
    /// Innermost frame of the interrupted code.
    pub pc: Option<u32>,
    pub function: Option<String>,
    pub location: Option<SourceLocation>,
}

/// Cycles the core ran between the last two stops.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CycleDelta {
    pub cycles: u32,
    pub core_hz: Option<u32>,
    pub microseconds: Option<f64>,
    /// The 32-bit counter may have wrapped, so `cycles` is only a remainder.
    pub wrapped: bool,
}

#[derive(Default)]
struct CycleTracker {
    at_last_stop: Option<u32>,
    run_started: Option<Instant>,
    delta: Option<CycleDelta>,
}

/// Bits for one exception filter of the editor.
fn catch_bits(filter: &str) -> u32 {
    match filter {
        "hardfault" => VC_HARDFAULT,
        "busfault" => VC_BUSFAULT,
        "memmanage" => VC_MEMMANAGE,
        "usagefault" => VC_USAGEFAULT,
        _ => 0,
    }
}

fn default_catch(options: &LaunchOptions) -> u32 {
    if options.project.debug.vector_catch {
        VC_ALL_FAULTS
    } else {
        0
    }
}

/// Register from a name shown in the Registers scope.
fn register_named(name: &str) -> Option<Reg> {
    Some(match name {
        "sp" => Reg::SP,
        "lr" => Reg::LR,
        "pc" => Reg::PC,
        "xpsr" => Reg::Xpsr,
        "msp" => Reg::Msp,
        "psp" => Reg::Psp,
        "primask" => Reg::Primask,
        "basepri" => Reg::Basepri,
        "faultmask" => Reg::Faultmask,
        "control" => Reg::Control,
        "fpscr" => Reg::Fpscr,
        other => {
            if let Some(number) = other.strip_prefix('r') {
                Reg::R(number.parse().ok().filter(|n| *n < 13)?)
            } else {
                Reg::S(other.strip_prefix('s')?.parse().ok().filter(|n| *n < 32)?)
            }
        }
    })
}

/// Write a scalar to the target, keeping neighbouring bit-fields intact.
pub fn write_scalar(
    handle: &ProbeHandle,
    place: Place,
    scalar: Scalar,
    text: &str,
) -> std::result::Result<(), String> {
    let value = scalar.encode(text)?;
    write_scalar_bits(handle, place, scalar, value)
}

/// Write an already-encoded scalar value to the target, keeping neighbouring bit-fields intact.
pub fn write_scalar_bits(
    handle: &ProbeHandle,
    place: Place,
    scalar: Scalar,
    value: u64,
) -> std::result::Result<(), String> {
    let size = scalar.size as usize;
    handle
        .call_blocking(Priority::P0, move |probe: &mut dyn Probe| {
            let mut bytes = value.to_le_bytes();
            if let Some(bits) = scalar.bits {
                let mut current = [0u8; 8];
                probe.read_mem(place.addr, &mut current[..size], AccessWidth::U8)?;
                let old = u64::from_le_bytes(current);
                let mask = (u64::MAX >> (64 - bits.bit_size)) << bits.bit_offset;
                bytes = ((old & !mask) | ((value << bits.bit_offset) & mask)).to_le_bytes();
            }
            match size {
                4 if place.addr.is_multiple_of(4) => probe.write_u32(
                    place.addr,
                    u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]),
                ),
                _ => probe.write_mem(place.addr, &bytes[..size], AccessWidth::U8),
            }
        })
        .map_err(|error| error.to_string())?
        .map_err(|error| error.to_string())
}

fn access_width(bits: u32) -> AccessWidth {
    match bits {
        8 => AccessWidth::U8,
        16 => AccessWidth::U16,
        _ => AccessWidth::U32,
    }
}

/// Variables by name, plus the bytes backing register-held ones.
type NamedLocals = (Vec<(String, Place)>, Vec<u8>);

/// A frame's variables, plus the bytes backing register-held ones.
type FrameLocals = (Vec<(LocalVar, Option<Place>)>, Vec<u8>);

pub struct Session {
    actor: ProbeActor,
    pub(crate) handle: ProbeHandle,
    pub(crate) db: Arc<SymbolDb>,
    pub(crate) rtos: Option<FreeRtos>,
    pub(crate) options: LaunchOptions,
    pub(crate) target: TargetInfo,
    pub(crate) profile: Option<DeviceProfile>,
    pub(crate) state: RunState,
    pub(crate) breakpoints: BreakpointTable,
    pub(crate) memory: HaltedMemory,
    pub(crate) cancel: Arc<AtomicBool>,
    /// Inlined frames not yet stepped into at the current stop.
    pub(crate) hidden_inline: usize,
    /// Source position of the breakpoint that caused the current stop.
    pub(crate) stop_location: Option<SourceLocation>,
    stack: Option<UnwoundStack>,
    float_registers: Option<Option<[u32; 32]>>,
    visualizers: Option<Visualizers>,
    /// DEMCR vector-catch bits in effect.
    fault_catch: u32,
    /// Log messages from breakpoints, waiting to be shown.
    output: Vec<String>,
    /// The next unwind only needs the innermost frame.
    shallow_unwind: bool,
    fpu: Option<bool>,
    cycles: CycleTracker,
    peripherals: Option<Arc<svd::Device>>,
    auto_resuming: bool,
    last_halted_snapshot: Option<(u32, u32, u32, u32)>,
    /// The step that outlasted its wait, if one is under way.
    pub(crate) detached: Option<DetachedRun>,
}

fn load_symbols(path: &Path) -> Result<Arc<SymbolDb>> {
    Ok(Arc::new(SymbolDb::load(path, &LoadOptions::default())?))
}

fn select_profile(options: &LaunchOptions) -> Option<DeviceProfile> {
    let family = options
        .project
        .target
        .family
        .as_deref()
        .or_else(|| family_for_device(&options.connect.device))?;
    match DeviceProfile::builtin(family) {
        Ok(profile) => Some(profile),
        Err(error) => {
            tracing::warn!("no device profile: {error}");
            None
        }
    }
}

impl Session {
    /// Take over an opened probe and connect to the target.
    pub fn start(probe: Box<dyn Probe>, options: LaunchOptions) -> Result<Self> {
        let db = load_symbols(&options.elf)?;
        let actor = ProbeActor::spawn(probe);
        let handle = actor.handle();
        let connect = options.connect.clone();
        let target = handle.call_blocking(Priority::P0, move |probe: &mut dyn Probe| {
            probe.connect(&connect)
        })??;
        let halted = target.halted;
        let rtos = FreeRtos::detect(&db)?;
        let profile = select_profile(&options);
        let memory = HaltedMemory::new(handle.clone());
        let breakpoints = BreakpointTable::new(
            target.hw_breakpoints as usize,
            options.project.debug.flash_breakpoints,
        );
        let mut session = Self {
            actor,
            handle,
            db,
            rtos,
            options,
            target,
            profile,
            state: if halted {
                RunState::Halted
            } else {
                RunState::Running
            },
            breakpoints,
            memory,
            cancel: Arc::new(AtomicBool::new(false)),
            hidden_inline: 0,
            stop_location: None,
            stack: None,
            float_registers: None,
            visualizers: None,
            fault_catch: 0,
            output: Vec::new(),
            shallow_unwind: false,
            fpu: None,
            cycles: CycleTracker::default(),
            peripherals: None,
            auto_resuming: false,
            last_halted_snapshot: None,
            detached: None,
        };
        session.fault_catch = default_catch(&session.options);
        session.visualizers = session.load_visualizers();
        session.configure_target()?;
        session.check_existing_comparators();
        if halted {
            session.refresh_halted_snapshot();
        }
        Ok(session)
    }

    /// True if an open session can serve `options` without reconnecting.
    pub fn matches(&self, options: &LaunchOptions) -> bool {
        self.options.connect.device == options.connect.device
            && self.options.connect.speed_khz == options.connect.speed_khz
            && self.options.connect.interface == options.connect.interface
            && self.options.probe.dll == options.probe.dll
            && self.options.probe.serial == options.probe.serial
    }

    /// Adopt new launch settings on a kept-alive connection.
    pub fn reconfigure(&mut self, options: LaunchOptions) -> Result<()> {
        self.db = load_symbols(&options.elf)?;
        self.rtos = FreeRtos::detect(&self.db)?;
        self.options = options;
        self.breakpoints
            .set_policy(self.options.project.debug.flash_breakpoints);
        self.fault_catch = default_catch(&self.options);
        self.cycles = CycleTracker::default();
        self.peripherals = None;
        self.visualizers = self.load_visualizers();
        self.profile = select_profile(&self.options);
        self.state = if self.probe_is_halted()? {
            RunState::Halted
        } else {
            RunState::Running
        };
        self.last_halted_snapshot = None;
        self.invalidate();
        self.configure_target()?;
        if self.state == RunState::Halted {
            self.refresh_halted_snapshot();
        }
        Ok(())
    }

    /// Stop and hand the probe back.
    pub fn shutdown(self) -> Result<Box<dyn Probe>> {
        Ok(self.actor.shutdown()?)
    }

    pub fn probe_handle(&self) -> ProbeHandle {
        self.handle.clone()
    }

    pub fn options(&self) -> &LaunchOptions {
        &self.options
    }

    pub fn device_profile(&self) -> Option<&DeviceProfile> {
        self.profile.as_ref()
    }

    pub fn symbols(&self) -> &Arc<SymbolDb> {
        &self.db
    }

    pub fn target(&self) -> &TargetInfo {
        &self.target
    }

    pub fn state(&self) -> &RunState {
        &self.state
    }

    /// Share a flag another thread sets to abandon a long operation.
    pub fn set_cancel_token(&mut self, token: Arc<AtomicBool>) {
        self.cancel = token;
    }

    /// Record that the target can no longer be treated as running.
    pub fn mark_halted(&mut self) {
        self.state = RunState::Halted;
        self.refresh_halted_snapshot();
    }

    /// DAP thread id of whatever is executing.
    pub fn current_thread(&mut self) -> Result<i64> {
        Ok(self
            .current_task()?
            .filter(|tcb| *tcb != 0)
            .map_or(MAIN_THREAD, i64::from))
    }

    /// Thread id and human-readable label for the currently executing CPU/RTOS context.
    pub fn active_context_label(&mut self) -> Result<(i64, String)> {
        let exc = self.registers().map(|r| r.xpsr & IPSR_MASK).unwrap_or(0);
        let device = if exc >= 16 {
            self.peripherals().ok().flatten()
        } else {
            None
        };
        let exc_name = (exc != 0).then(|| format_exception_short(exc, device.as_deref()));

        if self.rtos.is_some() {
            let current_tcb = self.current_task()?.filter(|t| *t != 0);
            let task_name = if let Some(tcb) = current_tcb {
                self.tasks()?
                    .and_then(|tasks| tasks.into_iter().find(|t| t.tcb == tcb).map(|t| t.name))
            } else {
                None
            };
            let id = current_tcb.map_or(MAIN_THREAD, i64::from);
            let label = match (exc_name, task_name) {
                (Some(exc_label), Some(task)) => format!("{exc_label} · Task: {task}"),
                (Some(exc_label), None) => exc_label,
                (None, Some(task)) => format!("Task: {task}"),
                (None, None) => "main".to_owned(),
            };
            return Ok((id, label));
        }

        let label = match exc_name {
            Some(exc_label) => format!("main ({exc_label})"),
            None => "main".to_owned(),
        };
        Ok((MAIN_THREAD, label))
    }

    /// Remove every breakpoint and leave the target running or halted.
    pub fn end_debugging(&mut self, halt: bool) -> Result<()> {
        self.begin_stop_tracking();
        if self.state == RunState::Running {
            self.call(|probe| probe.halt())?;
            self.state = RunState::Halted;
        }
        self.drop_detached()?;
        let handle = self.handle.clone();
        self.breakpoints.clear(&handle)?;
        // With nobody attached, a catch would hang the target at the fault.
        self.fault_catch = 0;
        self.call(|probe| probe.modify_u32(DEMCR, 0, VC_ALL_FAULTS))?;
        self.invalidate();
        if !halt {
            self.last_halted_snapshot = None;
            self.call(|probe| probe.go())?;
            self.state = RunState::Running;
        }
        Ok(())
    }

    pub(crate) fn debug_config(&self) -> &DebugConfig {
        &self.options.project.debug
    }

    pub(crate) fn call<R, F>(&self, f: F) -> Result<R>
    where
        R: Send + 'static,
        F: FnOnce(&mut dyn Probe) -> probe_api::Result<R> + Send + 'static,
    {
        Ok(self.handle.call_blocking(Priority::P0, f)??)
    }

    fn configure_target(&self) -> Result<()> {
        let debug = self.debug_config();
        let settings = PostConnectOptions {
            vector_catch: self.fault_catch,
            freeze_on_halt: debug.freeze_on_halt.clone(),
        };
        let profile = self.profile.clone().unwrap_or_else(generic_profile);
        self.call(move |probe| apply_post_connect(probe, &profile, &settings))
    }

    fn load_visualizers(&self) -> Option<Visualizers> {
        let rules = match Visualizers::with_builtin() {
            Ok(rules) => rules,
            Err(error) => {
                tracing::warn!("cannot start the visualizer runtime: {error}");
                return None;
            }
        };
        for file in &self.options.project.visualizers {
            let path = match &self.options.workspace {
                Some(workspace) => workspace.join(file),
                None => PathBuf::from(file),
            };
            if let Err(error) = rules.load_file(&path) {
                tracing::warn!("cannot load {}: {error}", path.display());
            }
        }
        Some(rules)
    }

    pub(crate) fn invalidate(&mut self) {
        self.memory.clear();
        self.stack = None;
        self.float_registers = None;
    }

    fn probe_is_halted(&self) -> Result<bool> {
        self.call(|probe| {
            let dll_halted = probe.is_halted()?;
            if !dll_halted {
                // If another debugger halted via CoreSight while DLL cache is stale, DHCSR detects it.
                return Ok(probe
                    .read_u32(DHCSR)
                    .is_ok_and(|dhcsr| dhcsr & DHCSR_S_HALT != 0));
            }
            if let Ok(dhcsr) = probe.read_u32(DHCSR) {
                return Ok(dhcsr & DHCSR_S_HALT != 0);
            }
            Ok(true)
        })
    }

    pub(crate) fn registers(&self) -> Result<CoreRegisters> {
        self.call(|probe| {
            let mut ids: Vec<Reg> = (0..16).map(Reg::R).collect();
            ids.extend([Reg::Xpsr, Reg::Msp, Reg::Psp]);
            let mut values = [0u32; 19];
            probe.read_regs(&ids, &mut values)?;
            let mut registers = CoreRegisters {
                xpsr: values[16],
                msp: values[17],
                psp: values[18],
                ..CoreRegisters::default()
            };
            registers.r.copy_from_slice(&values[..16]);

            // Cross-check PC and SP via CoreSight DCRSR/DCRDR because J-Link DLL
            // can return stale cached registers when SEGGER Ozone shares the probe.
            if let Ok(dhcsr) = probe.read_u32(DHCSR) {
                if dhcsr & DHCSR_S_HALT != 0 {
                    if let (Ok(direct_pc), Ok(direct_sp)) = (
                        read_core_reg_direct(probe, 15),
                        read_core_reg_direct(probe, 13),
                    ) {
                        if (direct_pc & !1) != (registers.r[15] & !1)
                            || direct_sp != registers.r[13]
                        {
                            tracing::warn!(
                                cached_pc = registers.r[15],
                                direct_pc,
                                cached_sp = registers.r[13],
                                direct_sp,
                                "probe register cache stale vs CoreSight DCRDR; refreshing via CoreSight"
                            );
                            if let Ok(fresh) = read_all_core_regs_direct(probe) {
                                return Ok(fresh);
                            }
                        }
                    }
                }
            }
            Ok(registers)
        })
    }

    fn refresh_halted_snapshot(&mut self) {
        if self.state != RunState::Halted {
            self.last_halted_snapshot = None;
            return;
        }
        if let Ok(regs) = self.registers() {
            self.last_halted_snapshot = Some((regs.r[15] & !1, regs.r[13], regs.r[14], regs.xpsr));
        }
    }

    fn check_existing_comparators(&mut self) {
        let counts = self.call(|probe| {
            let mut fpb_active = 0usize;
            if let Ok(fp_ctrl) = probe.read_u32(FP_CTRL) {
                if fp_ctrl & 1 != 0 {
                    let num_code =
                        (((fp_ctrl >> 4) & 0xF) | (((fp_ctrl >> 12) & 0x7) << 4)).min(16);
                    for i in 0..num_code {
                        if probe.read_u32(FP_COMP0 + i * 4).unwrap_or(0) & 1 != 0 {
                            fpb_active += 1;
                        }
                    }
                }
            }
            let mut dwt_active = 0usize;
            if let Ok(dwt_ctrl) = probe.read_u32(DWT_CTRL) {
                let num_comp = (dwt_ctrl >> 28).min(16);
                for i in 0..num_comp {
                    let func = probe
                        .read_u32(DWT_FUNCTION0 + i * DWT_COMPARATOR_STRIDE)
                        .unwrap_or(0);
                    if func & 0xF != 0 {
                        dwt_active += 1;
                    }
                }
            }
            Ok((fpb_active, dwt_active))
        });
        if let Ok((fpb, dwt)) = counts {
            if fpb > 0 || dwt > 0 {
                self.output.push(format!(
                    "Warning: detected pre-existing hardware comparators ({fpb} FPB, {dwt} DWT) — another debugger (e.g. SEGGER Ozone) may be attached; available comparators are shared"
                ));
            }
        }
    }

    pub fn has_fpu(&mut self) -> bool {
        if let Some(known) = self.fpu {
            return known;
        }
        let part = (self.target.cpuid >> CPUID_PART_SHIFT) & CPUID_PART_MASK;
        let present = FPU_CAPABLE_PARTS.contains(&part)
            && self
                .call(|probe| probe.read_u32(MVFR0))
                .is_ok_and(|features| features != 0);
        self.fpu = Some(present);
        present
    }

    fn floats(&mut self) -> Option<[u32; 32]> {
        if let Some(known) = self.float_registers {
            return known;
        }
        if !self.has_fpu() {
            self.float_registers = Some(None);
            return None;
        }
        let read = self.call(|probe| {
            let ids: Vec<Reg> = (0..32).map(Reg::S).collect();
            let mut values = [0u32; 32];
            probe.read_regs(&ids, &mut values)?;
            Ok(values)
        });
        let known = read.ok();
        self.float_registers = Some(known);
        known
    }

    /// Take the reason for the current halt and clear it on the target.
    pub(crate) fn consume_halt_reason(&mut self, requested_pause: bool) -> Result<StopReason> {
        let dfsr = self.call(|probe| {
            let value = probe.read_u32(DFSR)?;
            probe.write_u32(DFSR, value & DFSR_ALL)?;
            Ok(value)
        })?;
        self.invalidate();
        self.state = RunState::Halted;
        if dfsr & DFSR_VCATCH != 0 {
            return Ok(StopReason::Fault);
        }
        if dfsr & DFSR_DWTTRAP != 0 {
            let ids = self.matched_data_breakpoints()?;
            if !ids.is_empty() || requested_pause {
                return Ok(StopReason::Data(ids));
            }
            return Ok(StopReason::External);
        }
        if dfsr & DFSR_BKPT != 0 {
            let pc = self.registers()?.r[15] & !1;
            let ids = self.breakpoints.ids_at(pc);
            self.stop_location = self.breakpoints.location_at(pc);
            return Ok(StopReason::Breakpoint(ids));
        }
        if dfsr & DFSR_EXTERNAL != 0 {
            return Ok(StopReason::External);
        }
        // DFSR.HALTED is also how a breakpoint on some J-Link paths appears.
        let pc = self.registers()?.r[15] & !1;
        let ids = self.breakpoints.ids_at(pc);
        if !ids.is_empty() && !requested_pause {
            self.stop_location = self.breakpoints.location_at(pc);
            return Ok(StopReason::Breakpoint(ids));
        }
        if requested_pause {
            Ok(StopReason::Pause)
        } else {
            Ok(StopReason::External)
        }
    }

    /// Which data breakpoints fired, from the comparators that matched.
    /// Reading DWT_FUNCTION clears its MATCHED bit.
    fn matched_data_breakpoints(&self) -> Result<Vec<i64>> {
        let matched = self.call(|probe| {
            let count = (probe.read_u32(DWT_CTRL)? >> 28).min(16);
            let mut addresses = Vec::new();
            for index in 0..count {
                let stride = index * DWT_COMPARATOR_STRIDE;
                if probe.read_u32(DWT_FUNCTION0 + stride)? & DWT_FUNCTION_MATCHED != 0 {
                    addresses.push(probe.read_u32(DWT_COMP0 + stride)?);
                }
            }
            Ok(addresses)
        })?;
        Ok(self.breakpoints.data_ids_for(&matched))
    }

    /// Log messages produced by breakpoints since the last call.
    pub fn take_output(&mut self) -> Vec<String> {
        std::mem::take(&mut self.output)
    }

    fn condition_holds(&mut self, id: i64, thread: i64) -> Result<bool> {
        let Some(condition) = self
            .breakpoints
            .behavior(id)
            .and_then(|behavior| behavior.condition.clone())
        else {
            return Ok(true);
        };
        let (locals, synthetic) = self.stop_locals_at(thread, 0)?;
        let db = self.db.clone();
        let mut memory = FrameMemory {
            inner: &mut self.memory,
            synthetic,
        };
        let value: Number = db.evaluate_number(&condition, &mut memory, &locals)?;
        Ok(value.is_true())
    }

    fn render_log(&mut self, parts: &[LogPart], thread: i64) -> String {
        let mut text = String::new();
        for part in parts {
            match part {
                LogPart::Text(literal) => text.push_str(literal),
                LogPart::Expression(expression) => {
                    match self.evaluate(expression, Some((thread, 0))) {
                        Ok(view) => text.push_str(&view.value),
                        Err(error) => text.push_str(&format!("<{error}>")),
                    }
                }
            }
        }
        text
    }

    /// Decide one breakpoint's hit. A condition that cannot be evaluated stops
    /// the target, because silently carrying on would hide the mistake.
    fn hit_wants_stop(&mut self, id: i64, thread: i64) -> Result<bool> {
        match self.condition_holds(id, thread) {
            Ok(true) => {}
            Ok(false) => return Ok(false),
            Err(error) => {
                self.output
                    .push(format!("breakpoint condition failed: {error}"));
                return Ok(true);
            }
        }
        let hits = self.breakpoints.count_hit(id);
        let Some(behavior) = self.breakpoints.behavior(id) else {
            return Ok(true);
        };
        if behavior.hit.is_some_and(|hit| !hit.matches(hits)) {
            return Ok(false);
        }
        let Some(parts) = behavior.log.clone() else {
            return Ok(true);
        };
        let message = self.render_log(&parts, thread);
        self.output.push(message);
        Ok(false)
    }

    /// Apply breakpoint conditions to a stop. Returns None if the target
    /// should simply continue, which the caller then does.
    pub(crate) fn filter_stop(&mut self, reason: StopReason) -> Result<Option<StopReason>> {
        let ids = match &reason {
            StopReason::Breakpoint(ids) | StopReason::Data(ids) => ids.clone(),
            _ => return Ok(Some(reason)),
        };
        let plain = ids.iter().all(|id| {
            self.breakpoints
                .behavior(*id)
                .is_none_or(|behavior| behavior.is_plain())
        });
        if plain {
            return Ok(Some(reason));
        }
        self.note_stop(&reason)?;
        let thread = self.current_thread()?;
        self.shallow_unwind = true;
        let mut wanted = Vec::new();
        let mut failure = None;
        for id in ids {
            match self.hit_wants_stop(id, thread) {
                Ok(true) => wanted.push(id),
                Ok(false) => {}
                Err(error) => {
                    failure = Some(error);
                    break;
                }
            }
        }
        // A partial stack must not be mistaken for the real one.
        self.shallow_unwind = false;
        self.stack = None;
        if let Some(error) = failure {
            return Err(error);
        }
        if wanted.is_empty() {
            return Ok(None);
        }
        Ok(Some(match reason {
            StopReason::Data(_) => StopReason::Data(wanted),
            _ => StopReason::Breakpoint(wanted),
        }))
    }

    /// Detect a stop while running. Call every 20 ms or so. Breakpoints that
    /// resume by themselves are served here without waiting for the next poll.
    /// True while breakpoints keep resuming, so the next poll should not wait.
    pub fn is_auto_resuming(&self) -> bool {
        self.auto_resuming
    }

    pub fn poll(&mut self) -> Result<Option<StopReason>> {
        if self.state != RunState::Running {
            return Ok(None);
        }
        let started = Instant::now();
        let mut resumed = false;
        loop {
            // A breakpoint that keeps resuming must not shut out editor requests.
            if resumed && started.elapsed() >= AUTO_RESUME_BURST {
                self.auto_resuming = true;
                return Ok(None);
            }
            let halted = self.handle.call_blocking(
                Priority::P1,
                |probe: &mut dyn Probe| -> probe_api::Result<bool> {
                    let dll_halted = probe.is_halted()?;
                    if let Ok(dhcsr) = probe.read_u32(DHCSR) {
                        return Ok(dhcsr & DHCSR_S_HALT != 0);
                    }
                    Ok(dll_halted)
                },
            )??;
            if !halted {
                if resumed {
                    continue;
                }
                self.auto_resuming = false;
                return Ok(None);
            }
            let reason = self.consume_halt_reason(false)?;
            if self.detached.is_some() {
                match self.resolve_detached(reason)? {
                    DetachedOutcome::Stop(reason) => {
                        self.auto_resuming = false;
                        self.note_stop(&reason)?;
                        return Ok(Some(reason));
                    }
                    DetachedOutcome::FinishLine => {
                        self.auto_resuming = false;
                        let reason = self.step(StepKind::Over)?;
                        return Ok((reason != StopReason::StillRunning).then_some(reason));
                    }
                    DetachedOutcome::Resume => {
                        self.begin_stop_tracking();
                        self.resume_raw()?;
                        resumed = true;
                        continue;
                    }
                }
            }
            match self.filter_stop(reason)? {
                Some(reason) => {
                    self.auto_resuming = false;
                    self.note_stop(&reason)?;
                    return Ok(Some(reason));
                }
                None => {
                    self.begin_stop_tracking();
                    self.resume_raw()?;
                    resumed = true;
                }
            }
        }
    }

    /// Detect external resume or single-stepping while `haltd` believes the core is halted.
    pub fn poll_halted(&mut self) -> Result<HaltedPoll> {
        if self.state != RunState::Halted {
            return Ok(HaltedPoll::Unchanged);
        }
        let snap = self.handle.call_blocking(
            Priority::P1,
            |probe: &mut dyn Probe| -> probe_api::Result<Option<(u32, u32, u32, u32)>> {
                let dll_halted = probe.is_halted()?;
                let dhcsr_halted = probe.read_u32(DHCSR).ok().map(|v| v & DHCSR_S_HALT != 0);
                let halted = match dhcsr_halted {
                    Some(h) => dll_halted && h,
                    None => dll_halted,
                };
                if !halted {
                    return Ok(None);
                }
                if dhcsr_halted == Some(true) {
                    if let (Ok(pc), Ok(sp), Ok(lr), Ok(xpsr)) = (
                        read_core_reg_direct(probe, 15),
                        read_core_reg_direct(probe, 13),
                        read_core_reg_direct(probe, 14),
                        read_core_reg_direct(probe, 16),
                    ) {
                        return Ok(Some((pc & !1, sp, lr, xpsr)));
                    }
                }
                let mut values = [0u32; 4];
                probe.read_regs(
                    &[Reg::R(15), Reg::R(13), Reg::R(14), Reg::Xpsr],
                    &mut values,
                )?;
                Ok(Some((values[0] & !1, values[1], values[2], values[3])))
            },
        )??;
        let Some(current) = snap else {
            self.last_halted_snapshot = None;
            self.invalidate();
            self.state = RunState::Running;
            self.begin_stop_tracking();
            return Ok(HaltedPoll::ResumedExternally);
        };
        let Some(previous) = self.last_halted_snapshot else {
            self.last_halted_snapshot = Some(current);
            return Ok(HaltedPoll::Unchanged);
        };
        if previous == current {
            return Ok(HaltedPoll::Unchanged);
        }
        self.last_halted_snapshot = Some(current);
        let mut reason = self.consume_halt_reason(false)?;
        if matches!(reason, StopReason::Pause) {
            reason = StopReason::External;
        }
        self.note_stop(&reason)?;
        Ok(HaltedPoll::MovedExternally(reason))
    }

    /// Synchronize stop metadata when attaching to an already-halted core.
    pub fn observe_stop(&mut self, reason: &StopReason) -> Result<()> {
        self.invalidate();
        self.state = if self.probe_is_halted()? {
            RunState::Halted
        } else {
            RunState::Running
        };
        if self.state == RunState::Halted {
            self.note_stop(reason)?;
        } else {
            self.last_halted_snapshot = None;
        }
        Ok(())
    }

    /// Work out which inlined frames to present at a new stop. A stop at the
    /// first instruction of an inlined body shows the call site first, so that
    /// step-into can enter it without running anything.
    pub(crate) fn note_stop(&mut self, reason: &StopReason) -> Result<()> {
        self.hidden_inline = 0;
        let pc = self.registers()?.r[15] & !1;
        let chain = self.db.scope_at(pc)?;
        let at_entry = chain
            .iter()
            .rev()
            .take_while(|frame| frame.is_inlined && frame.entry_pc == pc)
            .count();
        let hide = match reason {
            StopReason::Step => at_entry,
            StopReason::Breakpoint(_) => {
                // Stay in the body if that is where the breakpoint was set;
                // otherwise present the frame whose line it was set on.
                let here = self.db.line_at(pc)?;
                if self.stop_location == here {
                    0
                } else {
                    chain
                        .iter()
                        .rposition(|frame| {
                            frame.call_site.is_some() && frame.call_site == self.stop_location
                        })
                        .map_or(at_entry, |position| chain.len() - position)
                }
            }
            _ => 0,
        };
        self.hidden_inline = hide;
        self.refresh_halted_snapshot();
        Ok(())
    }

    pub(crate) fn begin_stop_tracking(&mut self) {
        self.stop_location = None;
        self.hidden_inline = 0;
    }

    pub fn pause(&mut self) -> Result<StopReason> {
        self.call(|probe| probe.halt())?;
        self.drop_detached()?;
        let reason = self.consume_halt_reason(true)?;
        self.note_stop(&reason)?;
        Ok(reason)
    }

    /// Start the target, stepping off any breakpoint at the current PC.
    pub fn resume(&mut self) -> Result<()> {
        self.begin_stop_tracking();
        self.note_run_started();
        self.resume_raw()
    }

    pub(crate) fn resume_raw(&mut self) -> Result<()> {
        if self.state == RunState::Halted {
            let pc = self.registers()?.r[15] & !1;
            if self.breakpoints.has_address(pc) {
                self.call(|probe| probe.step_instr())?;
                // Stepping into a fault or breakpoint should still be noticed.
            }
        }
        self.last_halted_snapshot = None;
        self.invalidate();
        self.call(|probe| probe.go())?;
        self.state = RunState::Running;
        Ok(())
    }

    /// Wait until the core halts, the wait is cancelled, or `timeout` passes.
    pub(crate) fn wait_halted(&mut self, timeout: Option<Duration>) -> Result<Waited> {
        let started = Instant::now();
        loop {
            if self.probe_is_halted()? {
                return Ok(Waited::Halted);
            }
            if self.cancel.swap(false, Ordering::SeqCst) {
                return Ok(Waited::Cancelled);
            }
            if timeout.is_some_and(|limit| started.elapsed() > limit) {
                return Ok(Waited::TimedOut);
            }
            std::thread::sleep(HALT_POLL);
        }
    }

    /// Reset and leave the core halted at the reset vector.
    pub fn reset_halt(&mut self) -> Result<()> {
        if self.detached.is_some() && self.state == RunState::Running {
            self.call(|probe| probe.halt())?;
            self.state = RunState::Halted;
        }
        self.drop_detached()?;
        self.begin_stop_tracking();
        self.call(|probe| probe.reset(ResetKind::Normal, true))?;
        self.configure_target()?;
        self.invalidate_rtt_block()?;
        self.call(|probe| {
            let value = probe.read_u32(DFSR)?;
            probe.write_u32(DFSR, value & DFSR_ALL)
        })?;
        self.breakpoints.reset_hits();
        self.cycles = CycleTracker::default();
        self.invalidate();
        self.state = RunState::Halted;
        self.refresh_halted_snapshot();
        Ok(())
    }

    /// RAM survives a reset, so an old RTT control block would look valid until
    /// the new firmware gets round to replacing it.
    fn invalidate_rtt_block(&self) -> Result<()> {
        if let Some((address, _)) = self.db.elf_symbol("_SEGGER_RTT") {
            self.call(move |probe| probe.write_mem(address, &[0u8; 16], AccessWidth::U8))?;
        }
        Ok(())
    }

    pub fn flash(
        &mut self,
        progress: impl Fn(probe_api::Progress) + Send + 'static,
    ) -> Result<Duration> {
        let path = self.options.elf.clone();
        let stats = self.call(move |probe| probe.download(&path, &progress))?;
        self.invalidate();
        Ok(stats.elapsed)
    }

    /// Run to the end of `main`'s prologue.
    pub fn run_to_main(&mut self) -> Result<StopReason> {
        let main = self
            .db
            .functions("main")
            .into_iter()
            .next()
            .ok_or_else(|| EngineError::Other("the image has no main function".to_owned()))?;
        let target = self.db.after_prologue(main.low, main.high)?;
        let handle = self.place_temporary(target, BpKind::Hw)?;
        let outcome = self.run_until_wanted_stop(target);
        self.remove_temporary(handle)?;
        let result = outcome?;
        self.note_stop(&result)?;
        Ok(result)
    }

    /// Run to `target`, passing over breakpoints whose conditions say so.
    fn run_until_wanted_stop(&mut self, target: u32) -> Result<StopReason> {
        // A pause that has already been handled must not cancel this run.
        self.cancel.store(false, Ordering::SeqCst);
        loop {
            if !self.go_and_wait()? {
                self.call(|probe| probe.halt())?;
                self.consume_halt_reason(true)?;
                return Ok(StopReason::Pause);
            }
            let reason = self.consume_halt_reason(false)?;
            let at_target = self.registers()?.r[15] & !1 == target;
            return Ok(match self.filter_stop(reason)? {
                Some(StopReason::Fault) => StopReason::Fault,
                Some(StopReason::Data(ids)) => StopReason::Data(ids),
                Some(StopReason::Breakpoint(ids)) if !ids.is_empty() => StopReason::Breakpoint(ids),
                Some(_) => StopReason::Entry,
                None if at_target => StopReason::Entry,
                None => {
                    self.begin_stop_tracking();
                    continue;
                }
            });
        }
    }

    pub(crate) fn go_and_wait(&mut self) -> Result<bool> {
        self.resume_raw()?;
        Ok(self.wait_halted(None)? == Waited::Halted)
    }

    pub(crate) fn place_temporary(&self, address: u32, kind: BpKind) -> Result<BpHandle> {
        self.call(move |probe| probe.set_bp(address, kind))
    }

    pub(crate) fn remove_temporary(&self, handle: BpHandle) -> Result<()> {
        self.call(move |probe| probe.clear_bp(handle))
    }

    /// Run `action` with the core halted, restoring its run state afterwards.
    fn while_halted<R>(&mut self, action: impl FnOnce(&mut Self) -> Result<R>) -> Result<R> {
        let was_running = self.state == RunState::Running;
        if was_running {
            self.call(|probe| probe.halt())?;
        }
        let result = action(self);
        if was_running {
            self.call(|probe| probe.go())?;
        }
        result
    }

    /// Replace the breakpoints in `file`.
    pub fn set_source_breakpoints(
        &mut self,
        file: &str,
        lines: &[LineBreakpoint],
    ) -> Result<Vec<ResolvedBreakpoint>> {
        let file = file.to_owned();
        let lines = lines.to_vec();
        self.while_halted(move |session| {
            let db = session.db.clone();
            let handle = session.handle.clone();
            Ok(session
                .breakpoints
                .set_source(&db, &handle, &file, &lines)?)
        })
    }

    /// Replace the function breakpoints.
    pub fn set_function_breakpoints(
        &mut self,
        functions: &[FunctionBreakpoint],
    ) -> Result<Vec<ResolvedBreakpoint>> {
        let functions = functions.to_vec();
        self.while_halted(move |session| {
            let db = session.db.clone();
            let handle = session.handle.clone();
            Ok(session
                .breakpoints
                .set_functions(&db, &handle, &functions)?)
        })
    }

    /// Replace the data breakpoints.
    pub fn set_data_breakpoints(
        &mut self,
        requests: &[DataRequest],
    ) -> Result<Vec<ResolvedBreakpoint>> {
        let requests = requests.to_vec();
        self.while_halted(move |session| {
            let handle = session.handle.clone();
            Ok(session.breakpoints.set_data(&handle, &requests)?)
        })
    }

    /// Choose which faults halt on entry. A project with `vectorCatch: false`
    /// stays off whatever the editor sends.
    pub fn set_exception_filters(&mut self, filters: &[String]) -> Result<()> {
        let wanted = filters.iter().fold(0, |bits, name| bits | catch_bits(name));
        self.fault_catch = if self.debug_config().vector_catch {
            wanted
        } else {
            0
        };
        let bits = self.fault_catch;
        self.call(move |probe| probe.modify_u32(DEMCR, bits, VC_ALL_FAULTS & !bits))
    }

    /// Explain the fault the core is in.
    pub fn fault_report(&mut self) -> Result<FaultReport> {
        let registers = self.call(|probe| FaultRegisters::read(probe))?;
        let exception = self.registers()?.xpsr & IPSR_MASK;
        let thread = self.current_thread()?;
        let frames = self.stack_trace(thread).unwrap_or_default();
        // The frame below the exception boundary is the code that faulted. If
        // stacking failed there is no boundary, and the top frame is all we have.
        let boundary = frames.iter().position(|frame| frame.is_exception);
        let site = boundary
            .and_then(|index| frames.get(index + 1))
            .or(frames.first());
        // A bad branch faults at an address with no code, so the source line
        // is the nearest caller that has one.
        let source = boundary
            .and_then(|index| frames[index + 1..].iter().find(|f| f.location.is_some()))
            .or(site);
        Ok(FaultReport {
            exception,
            exception_name: fault::exception_name(exception),
            summary: fault::summarize(exception, &registers),
            causes: fault::decode_causes(&registers),
            address: fault::fault_address(&registers),
            imprecise: fault::is_imprecise(&registers),
            registers,
            thread,
            pc: site.map(|frame| frame.pc),
            function: source.map(|frame| frame.name.clone()),
            location: source.and_then(|frame| frame.location.clone()),
        })
    }

    pub(crate) fn note_run_started(&mut self) {
        self.cycles.run_started = Some(Instant::now());
    }

    /// `SystemCoreClock` as the firmware last set it.
    fn core_clock(&mut self) -> Option<u32> {
        let (address, _) = self.db.elf_symbol("SystemCoreClock")?;
        let mut bytes = [0u8; 4];
        self.memory.read(address, &mut bytes).ok()?;
        Some(u32::from_le_bytes(bytes)).filter(|hz| *hz != 0)
    }

    /// Update the cycle count at a stop reported to the user.
    pub fn note_cycles(&mut self) -> Result<Option<CycleDelta>> {
        let now = self.call(|probe| probe.read_u32(DWT_CYCCNT))?;
        let previous = self.cycles.at_last_stop.replace(now);
        let ran_for = self.cycles.run_started.take().map(|start| start.elapsed());
        let Some(previous) = previous else {
            return Ok(None);
        };
        let cycles = now.wrapping_sub(previous);
        let core_hz = self.core_clock();
        let wrapped = match (core_hz, ran_for) {
            (Some(hz), Some(duration)) => {
                duration.as_secs_f64() * f64::from(hz) >= f64::from(u32::MAX)
            }
            _ => false,
        };
        let delta = CycleDelta {
            cycles,
            core_hz,
            microseconds: core_hz.map(|hz| f64::from(cycles) * 1e6 / f64::from(hz)),
            wrapped,
        };
        self.cycles.delta = Some(delta);
        Ok(Some(delta))
    }

    pub fn last_cycles(&self) -> Option<CycleDelta> {
        self.cycles.delta
    }

    /// Read as much of a range as the target allows, from its start.
    pub fn read_memory(&mut self, address: u32, count: usize) -> Result<Vec<u8>> {
        let mut bytes = Vec::with_capacity(count);
        let mut chunk = MEMORY_CHUNK;
        while bytes.len() < count {
            let length = chunk.min(count - bytes.len());
            let at = address.wrapping_add(bytes.len() as u32);
            let read = self.call(move |probe| {
                let mut part = vec![0u8; length];
                probe.read_mem(at, &mut part, AccessWidth::Auto)?;
                Ok(part)
            });
            match read {
                Ok(part) => bytes.extend_from_slice(&part),
                // Close in on where the readable part ends.
                Err(_) if chunk > 1 => chunk /= 4,
                Err(EngineError::Probe(ProbeError::Memory { .. })) => break,
                Err(error) => return Err(error),
            }
        }
        Ok(bytes)
    }

    /// Change target memory. Flash goes through Build & Reload instead.
    pub fn write_memory(&mut self, address: u32, data: Vec<u8>) -> Result<()> {
        let end = u64::from(address) + data.len() as u64;
        let in_flash = self.profile.as_ref().is_some_and(|profile| {
            profile.memory.iter().any(|region| {
                region.kind == "flash"
                    && u64::from(address) < u64::from(region.start) + u64::from(region.size)
                    && end > u64::from(region.start)
            })
        });
        if in_flash {
            return Err(EngineError::Other(
                "flash cannot be changed from the memory view".to_owned(),
            ));
        }
        self.call(move |probe| probe.write_mem(address, &data, AccessWidth::Auto))?;
        self.invalidate();
        Ok(())
    }

    /// Set a variable from text.
    pub fn write_place(&mut self, place: Place, text: &str) -> Result<()> {
        if place.addr >= SYNTHETIC_BASE {
            return Err(EngineError::Other(
                "this value is held in a register and cannot be changed here".to_owned(),
            ));
        }
        let scalar = Scalar::of(&self.db, &place)
            .ok_or_else(|| EngineError::Other("only numeric values can be set".to_owned()))?;
        let value = self
            .db
            .encode_place_value(&place, text)
            .map_err(EngineError::Other)?;
        write_scalar_bits(&self.handle, place, scalar, value).map_err(EngineError::Other)?;
        self.invalidate();
        Ok(())
    }

    /// Set a register of the innermost frame.
    pub fn write_register(
        &mut self,
        thread: i64,
        frame: usize,
        name: &str,
        text: &str,
    ) -> Result<u32> {
        let register = register_named(name)
            .ok_or_else(|| EngineError::Other(format!("{name} cannot be set")))?;
        if !self.is_live_frame(thread, frame)? {
            return Err(EngineError::Other(
                "only the running frame's registers can be set".to_owned(),
            ));
        }
        let scalar = Scalar {
            kind: ScalarKind::Unsigned,
            size: 4,
            bits: None,
        };
        let value = scalar.encode(text).map_err(EngineError::Other)? as u32;
        self.call(move |probe| probe.write_reg(register, value))?;
        self.invalidate();
        Ok(value)
    }

    fn is_live_frame(&mut self, thread: i64, frame: usize) -> Result<bool> {
        let is_live = self
            .ensure_stack(thread)?
            .display
            .get(frame)
            .is_some_and(|display| !display.is_exception && !display.is_label && display.real == 0);
        Ok(is_live && self.is_current_thread(thread)?)
    }

    /// The SVD description, read on first use. None if the device has none.
    pub fn peripherals(&mut self) -> Result<Option<Arc<svd::Device>>> {
        if self.peripherals.is_none() {
            let configured =
                self.options
                    .project
                    .svd
                    .as_ref()
                    .map(|file| match &self.options.workspace {
                        Some(workspace) => workspace.join(file),
                        None => PathBuf::from(file),
                    });
            let Some(path) =
                configured.or_else(|| svd::find_for_device(&self.options.connect.device))
            else {
                return Ok(None);
            };
            let device = svd::load(&path).map_err(EngineError::Other)?;
            self.peripherals = Some(Arc::new(device));
        }
        Ok(self.peripherals.clone())
    }

    pub fn read_peripheral_register(&self, register: &svd::Register) -> Result<u32> {
        let (address, bits) = (register.address, register.bits);
        self.call(move |probe| {
            let mut bytes = [0u8; 4];
            let size = (bits / 8).clamp(1, 4) as usize;
            probe.read_mem(address, &mut bytes[..size], access_width(bits))?;
            Ok(u32::from_le_bytes(bytes))
        })
    }

    pub fn write_peripheral_register(
        &mut self,
        register: &svd::Register,
        value: u32,
    ) -> Result<()> {
        let (address, bits) = (register.address, register.bits);
        self.call(move |probe| {
            let size = (bits / 8).clamp(1, 4) as usize;
            probe.write_mem(address, &value.to_le_bytes()[..size], access_width(bits))
        })?;
        self.invalidate();
        Ok(())
    }

    pub fn breakpoint_placements(&self) -> Vec<Placement> {
        self.breakpoints.placements()
    }

    /// Build & Reload: program the image and re-bind everything to it.
    pub fn reload(
        &mut self,
        elf: Option<PathBuf>,
        progress: impl Fn(probe_api::Progress) + Send + 'static,
    ) -> Result<Vec<ResolvedBreakpoint>> {
        if let Some(elf) = elf {
            self.options.elf = elf;
        }
        if self.state == RunState::Running {
            self.call(|probe| probe.halt())?;
            self.state = RunState::Halted;
        }
        self.drop_detached()?;
        // Removed first so stale BKPTs do not disturb the sector comparison.
        let handle = self.handle.clone();
        self.breakpoints.unplace_all(&handle)?;

        let path = self.options.elf.clone();
        let (download, symbols) = std::thread::scope(|scope| {
            let loader = scope.spawn(|| load_symbols(&path));
            let progress_path = path.clone();
            let download = self.call(move |probe| probe.download(&progress_path, &progress));
            (download, loader.join())
        });
        download?;
        self.db =
            symbols.map_err(|_| EngineError::Other("symbol loader panicked".to_owned()))??;
        self.rtos = FreeRtos::detect(&self.db)?;
        self.reset_halt()?;
        let db = self.db.clone();
        let mut resolved = self.breakpoints.rebind(&db, &handle)?;
        let mut memory = HaltedMemory::new(handle.clone());
        resolved.extend(self.breakpoints.rebind_data(&handle, |expression| {
            let parsed = symbols::parse(expression).ok()?;
            match db.evaluate(&parsed, &mut memory).ok()? {
                symbols::Evaluated::Place(place) if place.bits.is_none() => {
                    Some((place.addr, db.place_size(&place).ok()? as u32))
                }
                _ => None,
            }
        })?);
        Ok(resolved)
    }

    // ---- Threads and frames ----

    pub fn tasks(&mut self) -> Result<Option<Vec<Task>>> {
        let Some(rtos) = &self.rtos else {
            return Ok(None);
        };
        Ok(Some(rtos.tasks(&mut self.memory)?.tasks))
    }

    fn start_registers(&mut self, thread: i64) -> Result<CoreRegisters> {
        // Walking the kernel's task lists takes tens of milliseconds, which
        // the thread that is running on the core never needs.
        if thread != MAIN_THREAD && !self.is_current_thread(thread)? {
            if let Some(rtos) = &self.rtos {
                let list = rtos.tasks(&mut self.memory)?.tasks;
                if let Some(task) = list
                    .iter()
                    .find(|task| i64::from(task.tcb) == thread && task.state != TaskState::Running)
                {
                    return Ok(rtos.saved_context(task, &mut self.memory)?);
                }
            }
        }
        self.registers()
    }

    pub(crate) fn current_task(&mut self) -> Result<Option<u32>> {
        let Some(rtos) = &self.rtos else {
            return Ok(None);
        };
        Ok(Some(rtos.current_tcb(&mut self.memory)?))
    }

    fn is_current_thread(&mut self, thread: i64) -> Result<bool> {
        Ok(thread == MAIN_THREAD
            || self
                .current_task()?
                .is_none_or(|current| i64::from(current) == thread))
    }

    fn ensure_stack(&mut self, thread: i64) -> Result<&UnwoundStack> {
        if self.stack.as_ref().is_none_or(|s| s.thread != thread) {
            let start = self.start_registers(thread)?;
            let limit = if self.shallow_unwind {
                1
            } else {
                MAX_DISPLAY_FRAMES
            };
            let (frames, stop_reason) = self.db.unwind_detailed(&start, &mut self.memory, limit)?;
            let current = self.is_current_thread(thread)?;
            let hidden = if current { self.hidden_inline } else { 0 };
            let override_location = self.stop_location.clone().filter(|_| current);
            let show_stop_label = !self.shallow_unwind;
            let display = self.expand_frames(
                &frames,
                hidden,
                override_location,
                &stop_reason,
                show_stop_label,
            )?;
            self.stack = Some(UnwoundStack {
                thread,
                frames,
                display,
                stop_reason,
            });
        }
        Ok(self.stack.as_ref().expect("just filled"))
    }

    /// Why unwinding stopped on the thread most recently asked about, if known.
    pub fn last_unwind_stop_reason(&self, thread: i64) -> Option<UnwindStopReason> {
        self.stack
            .as_ref()
            .filter(|s| s.thread == thread)
            .map(|s| s.stop_reason.clone())
    }

    fn expand_frames(
        &mut self,
        frames: &[symbols::StackFrame],
        hidden: usize,
        mut top_location: Option<SourceLocation>,
        stop_reason: &UnwindStopReason,
        show_stop_label: bool,
    ) -> Result<Vec<DisplayFrame>> {
        let has_external_irq = frames
            .iter()
            .any(|frame| matches!(frame.kind, FrameKind::Exception { number, .. } if number >= 16));
        let device = if has_external_irq {
            self.peripherals().ok().flatten()
        } else {
            None
        };
        let mut display = Vec::new();
        for (real, frame) in frames.iter().enumerate() {
            if let FrameKind::Exception { number, heuristic } = frame.kind {
                display.push(DisplayFrame {
                    name: format_exception_frame(number, heuristic, device.as_deref()),
                    location: None,
                    pc: frame.pc,
                    is_inlined: false,
                    is_exception: true,
                    is_label: false,
                    real,
                    chain_index: 0,
                });
                continue;
            }
            let chain = self.db.scope_at(frame.lookup_pc)?;
            if chain.is_empty() {
                display.push(DisplayFrame {
                    name: frame.function.clone().unwrap_or_else(|| "??".to_owned()),
                    location: None,
                    pc: frame.pc,
                    is_inlined: false,
                    is_exception: false,
                    is_label: false,
                    real,
                    chain_index: 0,
                });
                continue;
            }
            let skip = if real == 0 { hidden } else { 0 };
            let visible = chain.len().saturating_sub(skip).max(1);
            let mut location = if visible < chain.len() {
                chain[visible].call_site.clone()
            } else {
                self.db.line_at(frame.lookup_pc)?
            };
            if real == 0 {
                if let Some(top) = top_location.take() {
                    location = Some(top);
                }
            }
            for index in (0..visible).rev() {
                display.push(DisplayFrame {
                    name: chain[index].name.clone(),
                    location: location.clone(),
                    pc: frame.pc,
                    is_inlined: chain[index].is_inlined,
                    is_exception: false,
                    is_label: false,
                    real,
                    chain_index: index,
                });
                location = chain[index].call_site.clone();
            }
        }
        if show_stop_label {
            let stop_label = match *stop_reason {
                UnwindStopReason::Bottom | UnwindStopReason::LimitReached => None,
                UnwindStopReason::NoUnwindInfo { pc } => Some((
                    format!("<unwind stopped: no unwind info at 0x{pc:08x}>"),
                    pc,
                )),
                UnwindStopReason::MemoryError { pc, addr } => Some((
                    format!("<unwind stopped: memory read 0x{addr:08x} failed at 0x{pc:08x}>"),
                    pc,
                )),
                UnwindStopReason::CfaDidNotAdvance { pc, cfa } => Some((
                    format!("<unwind stopped: CFA stuck at 0x{cfa:08x} (pc 0x{pc:08x})>"),
                    pc,
                )),
                UnwindStopReason::ZeroReturnAddress { pc } => {
                    Some((format!("<unwind stopped: zero LR at 0x{pc:08x}>"), pc))
                }
            };
            if let Some((name, pc)) = stop_label {
                display.push(DisplayFrame {
                    name,
                    location: None,
                    pc,
                    is_inlined: false,
                    is_exception: false,
                    is_label: true,
                    real: frames.len(),
                    chain_index: 0,
                });
            }
        }
        Ok(display)
    }

    pub fn stack_trace(&mut self, thread: i64) -> Result<Vec<DisplayFrame>> {
        Ok(self.ensure_stack(thread)?.display.clone())
    }

    // ---- Variables ----

    /// Locals of a display frame, as named places in an overlay memory.
    fn frame_locals(&mut self, thread: i64, index: usize) -> Result<FrameLocals> {
        let stack = self.ensure_stack(thread)?;
        let Some(display) = stack.display.get(index).cloned() else {
            return Err(EngineError::Other(format!("no frame {index}")));
        };
        if display.is_exception || display.is_label {
            return Ok((Vec::new(), Vec::new()));
        }
        let real = stack.frames[display.real].clone();
        let floats = if display.real == 0 {
            self.floats()
        } else {
            None
        };
        let chain = self.db.scope_at(real.lookup_pc)?;
        let Some(scope) = chain.get(display.chain_index) else {
            return Ok((Vec::new(), Vec::new()));
        };
        let db = self.db.clone();
        let registers = FrameRegisters {
            pc: real.lookup_pc,
            r: &real.registers,
            s: floats.as_ref(),
            cfa: real.cfa,
        };

        let mut synthetic = Vec::new();
        let mut locals: Vec<(LocalVar, Option<Place>)> = Vec::new();
        for var in &scope.vars {
            if locals.iter().any(|(seen, _)| seen.name == var.name) {
                continue; // An inner block's variable shadows this one.
            }
            let place = match (var.ty, db.locate(var, &registers, &mut self.memory)) {
                (Some(ty), Ok(Located::Memory(addr))) => Some(Place::new(addr, ty)),
                (Some(ty), Ok(Located::Bytes(bytes))) => {
                    let addr = SYNTHETIC_BASE + synthetic.len() as u32;
                    let size = db.size_of(ty, 0).unwrap_or(bytes.len() as u64) as usize;
                    let mut value = bytes;
                    value.resize(size, 0);
                    synthetic.extend_from_slice(&value);
                    Some(Place::new(addr, ty))
                }
                _ => None,
            };
            locals.push((var.clone(), place));
        }
        Ok((locals, synthetic))
    }

    fn view(&mut self, name: String, place: Place, synthetic: Vec<u8>) -> VariableView {
        let db = self.db.clone();
        let type_name = db.type_name(place.ty);
        let mut memory = FrameMemory {
            inner: &mut self.memory,
            synthetic,
        };
        let visualized = self
            .visualizers
            .as_ref()
            .and_then(|rules| rules.apply(&db, &mut memory, place));
        if let Some(Visualized {
            summary: Some(summary),
            children,
        }) = &visualized
        {
            let expandable = children.as_ref().map_or_else(
                || {
                    db.children(&place, &mut memory)
                        .is_ok_and(|list| !list.is_empty())
                },
                |list| !list.is_empty(),
            );
            return VariableView {
                name,
                type_name,
                value: summary.clone(),
                place: Some(place),
                expandable,
            };
        }
        let too_big = db
            .place_size(&place)
            .map_or(true, |size| size > SUMMARY_LIMIT);
        let value = if too_big {
            "{...}".to_owned()
        } else {
            db.render_place(&place, &mut memory, &SUMMARY_OPTIONS)
                .unwrap_or_else(|error| format!("<{error}>"))
        };
        let expandable = db
            .children(&place, &mut memory)
            .map(|children| !children.is_empty())
            .unwrap_or(false);
        VariableView {
            name,
            type_name,
            value,
            place: Some(place),
            expandable,
        }
    }

    /// Bytes behind register-held locals of a frame, needed to expand them.
    pub fn synthetic_for(&mut self, thread: i64, index: usize) -> Result<Vec<u8>> {
        Ok(self.frame_locals(thread, index)?.1)
    }

    pub fn locals(&mut self, thread: i64, index: usize) -> Result<Vec<VariableView>> {
        let (locals, synthetic) = self.frame_locals(thread, index)?;
        let mut views = Vec::with_capacity(locals.len());
        for (var, place) in locals {
            views.push(match place {
                Some(place) => self.view(var.name, place, synthetic.clone()),
                None => VariableView {
                    name: var.name,
                    type_name: var.ty.map_or_else(String::new, |ty| self.db.type_name(ty)),
                    value: "<optimized out>".to_owned(),
                    place: None,
                    expandable: false,
                },
            });
        }
        Ok(views)
    }

    pub fn expand(&mut self, place: Place, synthetic: Vec<u8>) -> Result<Vec<VariableView>> {
        let db = self.db.clone();
        let children = {
            let mut memory = FrameMemory {
                inner: &mut self.memory,
                synthetic: synthetic.clone(),
            };
            let visualized = self
                .visualizers
                .as_ref()
                .and_then(|rules| rules.apply(&db, &mut memory, place))
                .and_then(|result| result.children);
            match visualized {
                Some(children) => children,
                None => db.children(&place, &mut memory)?,
            }
        };
        Ok(children
            .into_iter()
            .map(|(name, child)| self.view(name, child, synthetic.clone()))
            .collect())
    }

    /// Registers as a frame sees them. Registers the unwinder cannot recover
    /// are left out; special registers only exist for the running frame.
    pub fn registers_view(&mut self, thread: i64, index: usize) -> Result<Vec<(String, u32)>> {
        let stack = self.ensure_stack(thread)?;
        let Some(display) = stack.display.get(index) else {
            return Ok(Vec::new());
        };
        if display.is_label {
            return Ok(Vec::new());
        }
        let registers = stack.frames[display.real].registers;
        const NAMES: [&str; 16] = [
            "r0", "r1", "r2", "r3", "r4", "r5", "r6", "r7", "r8", "r9", "r10", "r11", "r12", "sp",
            "lr", "pc",
        ];
        let mut view: Vec<(String, u32)> = NAMES
            .iter()
            .zip(registers)
            .filter_map(|(name, value)| value.map(|value| ((*name).to_owned(), value)))
            .collect();
        if self.is_live_frame(thread, index)? {
            const SPECIAL: [(&str, Reg); 7] = [
                ("xpsr", Reg::Xpsr),
                ("msp", Reg::Msp),
                ("psp", Reg::Psp),
                ("primask", Reg::Primask),
                ("basepri", Reg::Basepri),
                ("faultmask", Reg::Faultmask),
                ("control", Reg::Control),
            ];
            let values = self.call(|probe| {
                let ids = SPECIAL.map(|(_, register)| register);
                let mut values = [0u32; SPECIAL.len()];
                probe.read_regs(&ids, &mut values)?;
                Ok(values)
            })?;
            view.extend(
                SPECIAL
                    .iter()
                    .zip(values)
                    .map(|((name, _), value)| ((*name).to_owned(), value)),
            );
        }
        Ok(view)
    }

    /// FPU state of the running frame: S0-S31 and FPSCR.
    pub fn fpu_view(&mut self, thread: i64, index: usize) -> Result<Option<([u32; 32], u32)>> {
        if !self.is_live_frame(thread, index)? {
            return Ok(None);
        }
        let Some(singles) = self.floats() else {
            return Ok(None);
        };
        let status = self.call(|probe| {
            let mut value = [0u32; 1];
            probe.read_regs(&[Reg::Fpscr], &mut value)?;
            Ok(value[0])
        })?;
        Ok(Some((singles, status)))
    }

    /// Variables of a frame as places, to find one by name.
    pub fn local_place(&mut self, thread: i64, index: usize, name: &str) -> Result<Option<Place>> {
        let (locals, _) = self.stop_locals_at(thread, index)?;
        Ok(locals
            .into_iter()
            .find(|(local, _)| local == name)
            .map(|(_, place)| place))
    }

    fn stop_locals_at(&mut self, thread: i64, index: usize) -> Result<NamedLocals> {
        let (locals, synthetic) = self.frame_locals(thread, index)?;
        Ok((
            locals
                .into_iter()
                .filter_map(|(var, place)| place.map(|place| (var.name, place)))
                .collect(),
            synthetic,
        ))
    }

    pub fn place_in_memory(place: &Place) -> bool {
        place.addr < SYNTHETIC_BASE
    }

    pub fn place_size(&self, place: &Place) -> Result<u32> {
        Ok(self.db.place_size(place)? as u32)
    }

    pub fn globals(&mut self) -> Vec<(String, Place)> {
        let db = self.db.clone();
        let mut out = Vec::new();
        for variable in db.all_globals() {
            if let Ok(Some(ty)) = db.variable_type(&variable) {
                out.push((variable.name, Place::new(variable.addr, ty)));
            }
        }
        out.dedup_by(|a, b| a.0 == b.0 && a.1 == b.1);
        out
    }

    pub fn view_global(&mut self, name: String, place: Place) -> VariableView {
        self.view(name, place, Vec::new())
    }

    /// Evaluate an expression in a frame, or globally without one. The target
    /// may be running: global reads go straight to memory.
    pub fn evaluate(
        &mut self,
        expression: &str,
        frame: Option<(i64, usize)>,
    ) -> Result<VariableView> {
        let (locals, synthetic) = match frame {
            Some((thread, index)) if self.state == RunState::Halted => {
                self.frame_locals(thread, index)?
            }
            _ => (Vec::new(), Vec::new()),
        };
        let named: Vec<(String, Place)> = locals
            .into_iter()
            .filter_map(|(var, place)| place.map(|place| (var.name, place)))
            .collect();
        if self.state == RunState::Running {
            self.memory.clear();
        }
        let db = self.db.clone();
        let parsed = symbols::parse(expression)?;
        let evaluated = {
            let mut memory = FrameMemory {
                inner: &mut self.memory,
                synthetic: synthetic.clone(),
            };
            db.evaluate_with(&parsed, &mut memory, &named)?
        };
        Ok(match evaluated {
            symbols::Evaluated::Place(place) => self.view(expression.to_owned(), place, synthetic),
            symbols::Evaluated::Value(number) => VariableView {
                name: expression.to_owned(),
                type_name: String::new(),
                value: number.to_string(),
                place: None,
                expandable: false,
            },
            symbols::Evaluated::Address { value, target } => VariableView {
                name: expression.to_owned(),
                type_name: format!("{}*", db.type_name(target)),
                value: format!("{value:#010x}"),
                place: None,
                expandable: false,
            },
        })
    }

    pub(crate) fn set_mask_interrupts(&self, enable: bool) -> Result<()> {
        let value =
            DHCSR_KEY | DHCSR_C_DEBUGEN | DHCSR_C_HALT | if enable { DHCSR_C_MASKINTS } else { 0 };
        self.call(move |probe| probe.write_u32(DHCSR, value))
    }
}

fn read_core_reg_direct(probe: &mut dyn Probe, reg_sel: u32) -> probe_api::Result<u32> {
    probe.write_u32(DCRSR, reg_sel & 0x7F)?;
    for _ in 0..32 {
        let dhcsr = probe.read_u32(DHCSR)?;
        if dhcsr & DHCSR_S_REGRDY != 0 {
            return probe.read_u32(DCRDR);
        }
    }
    Err(ProbeError::Other(format!(
        "CoreSight DCRDR read timeout for register selector {reg_sel}"
    )))
}

fn read_all_core_regs_direct(probe: &mut dyn Probe) -> probe_api::Result<CoreRegisters> {
    let mut registers = CoreRegisters::default();
    for (i, slot) in registers.r.iter_mut().enumerate() {
        *slot = read_core_reg_direct(probe, i as u32)?;
    }
    registers.xpsr = read_core_reg_direct(probe, 16)?;
    registers.msp = read_core_reg_direct(probe, 17)?;
    registers.psp = read_core_reg_direct(probe, 18)?;
    Ok(registers)
}

fn format_exception_short(number: u32, device: Option<&svd::Device>) -> String {
    match number {
        2 => "NMI".to_owned(),
        3 => "HardFault".to_owned(),
        4 => "MemManage".to_owned(),
        5 => "BusFault".to_owned(),
        6 => "UsageFault".to_owned(),
        11 => "SVCall".to_owned(),
        12 => "DebugMonitor".to_owned(),
        14 => "PendSV".to_owned(),
        15 => "SysTick".to_owned(),
        n if n >= 16 => {
            let irq = n - 16;
            match device.and_then(|d| d.interrupts.get(&irq)) {
                Some(name) => format!("IRQ {irq} ({name})"),
                None => format!("IRQ {irq}"),
            }
        }
        n => format!("exception {n}"),
    }
}

fn format_exception_frame(number: u32, heuristic: bool, device: Option<&svd::Device>) -> String {
    let base = format_exception_short(number, device);
    if heuristic {
        format!("<exception: {base} (heuristic)>")
    } else {
        format!("<exception: {base}>")
    }
}

fn generic_profile() -> DeviceProfile {
    DeviceProfile {
        family: "generic".to_owned(),
        memory: Vec::new(),
        flash_sector_size: 0,
        post_connect: Vec::new(),
        freeze: Default::default(),
    }
}
