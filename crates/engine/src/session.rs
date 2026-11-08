//! Debug session: run state, stop detection, stack frames and variables
//! (DESIGN §5.5). All probe access goes through the actor.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use probe_api::{
    apply_post_connect, AccessWidth, BpHandle, BpKind, ConnectCfg, DeviceProfile,
    PostConnectOptions, Probe, ProbeError, ProbeExt, Reg, ResetKind, TargetInfo,
};
use probe_jlink::actor::{Priority, ProbeActor, ProbeHandle};
use symbols::{
    CoreRegisters, FormatOptions, FrameKind, FrameRegisters, LoadOptions, LocalVar, Located,
    MemoryReader, Place, SourceLocation, SymbolDb, SymbolError,
};

use crate::breakpoints::{BreakpointTable, Placement, ResolvedBreakpoint};
use crate::project::{family_for_device, DebugConfig, ProjectConfig};
use crate::rtos::freertos::{FreeRtos, Task, TaskState};
use visualizer::{Visualized, Visualizers};

const DHCSR: u32 = 0xE000_EDF0;
const DHCSR_KEY: u32 = 0xA05F_0000;
const DHCSR_C_DEBUGEN: u32 = 1 << 0;
const DHCSR_C_HALT: u32 = 1 << 1;
const DHCSR_C_MASKINTS: u32 = 1 << 3;
const DFSR: u32 = 0xE000_ED30;
const DFSR_BKPT: u32 = 1 << 1;
const DFSR_DWTTRAP: u32 = 1 << 2;
const DFSR_VCATCH: u32 = 1 << 3;
const DFSR_EXTERNAL: u32 = 1 << 4;
const DFSR_ALL: u32 = 0x1F;
const HALT_POLL: Duration = Duration::from_millis(5);
/// Above the Cortex-M memory map's useful range; never read on the target.
const SYNTHETIC_BASE: u32 = 0xFFF0_0000;

/// Largest aggregate summarized on one line.
const SUMMARY_LIMIT: u64 = 4096;
const SUMMARY_OPTIONS: FormatOptions = FormatOptions {
    max_elements: 8,
    max_depth: 2,
};

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
    Data,
    Fault,
    External,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RunState {
    Running,
    Halted,
}

#[derive(Clone, Debug)]
pub struct LaunchOptions {
    pub connect: ConnectCfg,
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
    /// Index into the real frames.
    real: usize,
    /// Position in that frame's inline chain.
    chain_index: usize,
}

/// Real frames of the thread most recently asked about.
pub(crate) struct UnwoundStack {
    thread: i64,
    frames: Vec<symbols::StackFrame>,
    display: Vec<DisplayFrame>,
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
        };
        session.visualizers = session.load_visualizers();
        session.configure_target()?;
        Ok(session)
    }

    /// True if an open session can serve `options` without reconnecting.
    pub fn matches(&self, options: &LaunchOptions) -> bool {
        self.options.connect.device == options.connect.device
            && self.options.connect.speed_khz == options.connect.speed_khz
            && self.options.connect.interface == options.connect.interface
    }

    /// Adopt new launch settings on a kept-alive connection.
    pub fn reconfigure(&mut self, options: LaunchOptions) -> Result<()> {
        self.db = load_symbols(&options.elf)?;
        self.rtos = FreeRtos::detect(&self.db)?;
        self.options = options;
        self.visualizers = self.load_visualizers();
        self.profile = select_profile(&self.options);
        self.state = if self.probe_is_halted()? {
            RunState::Halted
        } else {
            RunState::Running
        };
        self.invalidate();
        self.configure_target()
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
    }

    /// DAP thread id of whatever is executing.
    pub fn current_thread(&mut self) -> Result<i64> {
        Ok(self
            .current_task()?
            .filter(|tcb| *tcb != 0)
            .map_or(MAIN_THREAD, i64::from))
    }

    /// Remove every breakpoint and leave the target running or halted.
    pub fn end_debugging(&mut self, halt: bool) -> Result<()> {
        self.begin_stop_tracking();
        if self.state == RunState::Running {
            self.call(|probe| probe.halt())?;
            self.state = RunState::Halted;
        }
        let handle = self.handle.clone();
        self.breakpoints.clear(&handle)?;
        self.invalidate();
        if !halt {
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
            vector_catch: debug.vector_catch,
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
        self.call(|probe| probe.is_halted())
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
            Ok(registers)
        })
    }

    fn floats(&mut self) -> Option<[u32; 32]> {
        if let Some(known) = self.float_registers {
            return known;
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
            return Ok(StopReason::Data);
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
        Ok(StopReason::Pause)
    }

    /// Detect a stop while running. Call every 20 ms or so.
    pub fn poll(&mut self) -> Result<Option<StopReason>> {
        if self.state != RunState::Running {
            return Ok(None);
        }
        let halted = self
            .handle
            .call_blocking(Priority::P1, |probe: &mut dyn Probe| probe.is_halted())??;
        if !halted {
            return Ok(None);
        }
        let reason = self.consume_halt_reason(false)?;
        self.note_stop(&reason)?;
        Ok(Some(reason))
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
        Ok(())
    }

    pub(crate) fn begin_stop_tracking(&mut self) {
        self.stop_location = None;
        self.hidden_inline = 0;
    }

    pub fn pause(&mut self) -> Result<StopReason> {
        self.call(|probe| probe.halt())?;
        let reason = self.consume_halt_reason(true)?;
        self.note_stop(&reason)?;
        Ok(reason)
    }

    /// Start the target, stepping off any breakpoint at the current PC.
    pub fn resume(&mut self) -> Result<()> {
        self.begin_stop_tracking();
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
        self.invalidate();
        self.call(|probe| probe.go())?;
        self.state = RunState::Running;
        Ok(())
    }

    /// Wait until halted, returning false if cancelled first.
    pub(crate) fn wait_halted(&mut self, timeout: Option<Duration>) -> Result<bool> {
        let started = Instant::now();
        loop {
            if self.probe_is_halted()? {
                return Ok(true);
            }
            if self.cancel.swap(false, Ordering::SeqCst) {
                return Ok(false);
            }
            if timeout.is_some_and(|limit| started.elapsed() > limit) {
                return Ok(false);
            }
            std::thread::sleep(HALT_POLL);
        }
    }

    /// Reset and leave the core halted at the reset vector.
    pub fn reset_halt(&mut self) -> Result<()> {
        self.begin_stop_tracking();
        self.call(|probe| probe.reset(ResetKind::Normal, true))?;
        self.configure_target()?;
        self.invalidate_rtt_block()?;
        self.call(|probe| {
            let value = probe.read_u32(DFSR)?;
            probe.write_u32(DFSR, value & DFSR_ALL)
        })?;
        self.invalidate();
        self.state = RunState::Halted;
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
        let outcome = self.go_and_wait();
        self.remove_temporary(handle)?;
        let reached = outcome?;
        if !reached {
            return Err(EngineError::Other("main was not reached".to_owned()));
        }
        let reason = self.consume_halt_reason(false)?;
        let result = match reason {
            StopReason::Fault | StopReason::Data => reason,
            StopReason::Breakpoint(ids) if !ids.is_empty() => StopReason::Breakpoint(ids),
            _ => StopReason::Entry,
        };
        self.note_stop(&result)?;
        Ok(result)
    }

    pub(crate) fn go_and_wait(&mut self) -> Result<bool> {
        self.resume_raw()?;
        self.wait_halted(None)
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
        lines: &[u32],
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
        names: &[String],
    ) -> Result<Vec<ResolvedBreakpoint>> {
        let names = names.to_vec();
        self.while_halted(move |session| {
            let db = session.db.clone();
            let handle = session.handle.clone();
            Ok(session.breakpoints.set_functions(&db, &handle, &names)?)
        })
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
        Ok(self.breakpoints.rebind(&db, &handle)?)
    }

    // ---- Threads and frames ----

    pub fn tasks(&mut self) -> Result<Option<Vec<Task>>> {
        let Some(rtos) = &self.rtos else {
            return Ok(None);
        };
        Ok(Some(rtos.tasks(&mut self.memory)?.tasks))
    }

    fn start_registers(&mut self, thread: i64) -> Result<CoreRegisters> {
        if thread != MAIN_THREAD {
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
            let frames = self.db.unwind(&start, &mut self.memory)?;
            let current = self.is_current_thread(thread)?;
            let hidden = if current { self.hidden_inline } else { 0 };
            let override_location = self.stop_location.clone().filter(|_| current);
            let display = self.expand_frames(&frames, hidden, override_location)?;
            self.stack = Some(UnwoundStack {
                thread,
                frames,
                display,
            });
        }
        Ok(self.stack.as_ref().expect("just filled"))
    }

    fn expand_frames(
        &self,
        frames: &[symbols::StackFrame],
        hidden: usize,
        mut top_location: Option<SourceLocation>,
    ) -> Result<Vec<DisplayFrame>> {
        let mut display = Vec::new();
        for (real, frame) in frames.iter().enumerate() {
            if let FrameKind::Exception { number } = frame.kind {
                display.push(DisplayFrame {
                    name: format!("<exception {number}>"),
                    location: None,
                    pc: frame.pc,
                    is_inlined: false,
                    is_exception: true,
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
                    real,
                    chain_index: index,
                });
                location = chain[index].call_site.clone();
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
        let real = stack.frames[display.real].clone();
        if display.is_exception {
            return Ok((Vec::new(), Vec::new()));
        }
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

    pub fn registers_view(&mut self, thread: i64, index: usize) -> Result<Vec<(String, u32)>> {
        let stack = self.ensure_stack(thread)?;
        let Some(display) = stack.display.get(index) else {
            return Ok(Vec::new());
        };
        let registers = stack.frames[display.real].registers;
        const NAMES: [&str; 16] = [
            "r0", "r1", "r2", "r3", "r4", "r5", "r6", "r7", "r8", "r9", "r10", "r11", "r12", "sp",
            "lr", "pc",
        ];
        Ok(NAMES
            .iter()
            .zip(registers)
            .filter_map(|(name, value)| value.map(|value| ((*name).to_owned(), value)))
            .collect())
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

fn generic_profile() -> DeviceProfile {
    DeviceProfile {
        family: "generic".to_owned(),
        memory: Vec::new(),
        flash_sector_size: 0,
        post_connect: Vec::new(),
        freeze: Default::default(),
    }
}
