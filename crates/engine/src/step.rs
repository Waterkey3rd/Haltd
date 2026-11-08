//! Source-level stepping for optimized code (DESIGN §5.5.3).

use std::sync::atomic::Ordering;

use probe_api::{BpHandle, BpKind};
use symbols::{ScopeFrame, SourceLocation};

use crate::project::FlashBreakpoints;
use crate::session::{EngineError, Result, RunState, Session, StopReason};

/// Instructions single-stepped before trying temporary breakpoints.
const SINGLE_STEPS_BEFORE_ESCAPE: u32 = 64;
/// Hard stop for a line that cannot be escaped with FPB comparators.
const MAX_SINGLE_STEPS: u32 = 20_000;
/// Instructions after which temporary flash breakpoints become acceptable.
/// Normal lines never get here, so ordinary stepping causes no flash wear.
const SINGLE_STEPS_BEFORE_FLASH_ESCAPE: u32 = 2_048;

const EXC_RETURN_MIN: u32 = 0xFFFF_FF00;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StepKind {
    Over,
    Into,
    Out,
}

#[derive(Clone, Debug)]
struct FrameState {
    pc: u32,
    cfa: u32,
    return_address: u32,
    /// Exception number, 0 in thread mode.
    exception: u32,
}

/// Why a run with temporary breakpoints ended.
enum RunOutcome {
    /// Stopped on one of ours, in the right context.
    Reached,
    /// Something else needs reporting.
    Stopped(StopReason),
}

fn prefix_of(short: &[u32], long: &[u32]) -> bool {
    long.len() >= short.len() && long[..short.len()] == *short
}

fn chain_ids(chain: &[ScopeFrame]) -> Vec<u32> {
    chain.iter().map(|frame| frame.die).collect()
}

impl Session {
    fn frame_state(&mut self) -> Result<FrameState> {
        let registers = self.registers()?;
        self.memory.clear();
        let (cfa, return_address) = self.db.frame_info(&registers, &mut self.memory)?;
        Ok(FrameState {
            pc: registers.r[15] & !1,
            exception: registers.xpsr & 0x1FF,
            cfa,
            return_address,
        })
    }

    fn single_step(&mut self) -> Result<()> {
        self.call(|probe| probe.step_instr())
    }

    fn cancelled(&self) -> bool {
        self.cancel.swap(false, Ordering::SeqCst)
    }

    /// Run until one of `addresses` is reached in the same task and exception
    /// context with `is_ours` true. Anything else that halts the core ends the
    /// run and is reported to the caller.
    fn run_to(
        &mut self,
        addresses: &[u32],
        kind: BpKind,
        is_ours: impl Fn(&FrameState) -> bool,
    ) -> Result<RunOutcome> {
        let task = self.current_task()?;
        let exception = self.frame_state()?.exception;
        let mut placed: Vec<BpHandle> = Vec::with_capacity(addresses.len());
        for address in addresses {
            placed.push(self.place_temporary(*address, kind)?);
        }
        let masked = self.debug_config().mask_interrupts_on_step;
        if masked {
            // Interrupts must run while the target is free-running.
            self.set_mask_interrupts(false)?;
        }
        let outcome = self.run_to_loop(addresses, exception, is_ours, task);
        for handle in placed {
            self.remove_temporary(handle)?;
        }
        if masked && *self.state() == RunState::Halted {
            self.set_mask_interrupts(true)?;
        }
        outcome
    }

    fn run_to_loop(
        &mut self,
        addresses: &[u32],
        exception: u32,
        is_ours: impl Fn(&FrameState) -> bool,
        task: Option<u32>,
    ) -> Result<RunOutcome> {
        loop {
            if !self.go_and_wait()? {
                self.call(|probe| probe.halt())?;
                self.consume_halt_reason(true)?;
                return Ok(RunOutcome::Stopped(StopReason::Pause));
            }
            let reason = self.consume_halt_reason(false)?;
            let state = self.frame_state()?;
            if matches!(reason, StopReason::Fault | StopReason::Data) {
                return Ok(RunOutcome::Stopped(reason));
            }
            if addresses.contains(&state.pc) {
                self.memory.clear();
                let same_context = state.exception == exception
                    && (task.is_none() || self.current_task()? == task);
                if same_context && is_ours(&state) {
                    return Ok(RunOutcome::Reached);
                }
                // A user breakpoint here still counts, even in another context.
                if matches!(&reason, StopReason::Breakpoint(ids) if !ids.is_empty()) {
                    return Ok(RunOutcome::Stopped(reason));
                }
                continue;
            }
            return Ok(RunOutcome::Stopped(reason));
        }
    }

    /// Finish the call whose return address and caller CFA are known.
    fn finish_call(&mut self, return_address: u32, caller_cfa: u32) -> Result<RunOutcome> {
        if return_address >= EXC_RETURN_MIN {
            // Returning from a handler: there is no code address to stop at.
            return Ok(RunOutcome::Stopped(StopReason::Step));
        }
        let target = return_address & !1;
        self.run_to(&[target], BpKind::Hw, move |state| state.cfa >= caller_cfa)
    }

    /// Statement addresses in a function chosen by `keep`, one per contiguous
    /// run of the same line, plus the return address.
    fn exit_addresses(
        &mut self,
        low: u32,
        high: u32,
        return_address: u32,
        keep: impl Fn(&[ScopeFrame], &SourceLocation) -> bool,
    ) -> Result<Vec<u32>> {
        let mut addresses = Vec::new();
        let mut previous: Option<SourceLocation> = None;
        for (address, location) in self.db.statements_in(low, high)? {
            if self.db.function_at(address).is_none_or(|f| f.low != low) {
                previous = None;
                continue;
            }
            let repeats = previous.as_ref() == Some(&location);
            let chain = self.db.scope_at(address)?;
            if !repeats && keep(&chain, &location) {
                addresses.push(address);
            }
            previous = Some(location);
        }
        if return_address < EXC_RETURN_MIN {
            addresses.push(return_address & !1);
        }
        addresses.sort_unstable();
        addresses.dedup();
        Ok(addresses)
    }

    fn free_comparators(&self) -> usize {
        let used = self
            .breakpoint_placements()
            .iter()
            .filter(|placement| !placement.is_flash)
            .count();
        (self.target.hw_breakpoints as usize).saturating_sub(used)
    }

    /// The kind of temporary breakpoint to escape with after `steps`, if any.
    /// FPB is preferred; flash is reserved for genuinely long-running lines
    /// (DESIGN §5.5.3, §15).
    fn escape_kind(&self, targets: usize, steps: u32) -> Option<BpKind> {
        if targets <= self.free_comparators() {
            Some(BpKind::Hw)
        } else if steps >= SINGLE_STEPS_BEFORE_FLASH_ESCAPE
            && self.debug_config().flash_breakpoints != FlashBreakpoints::Never
        {
            Some(BpKind::Flash)
        } else {
            None
        }
    }

    fn visible_location(&self, pc: u32, chain: &[ScopeFrame]) -> Result<Option<SourceLocation>> {
        if self.hidden_inline > 0 && self.hidden_inline < chain.len() {
            return Ok(chain[chain.len() - self.hidden_inline].call_site.clone());
        }
        Ok(self.db.line_at(pc)?)
    }

    /// Step, leaving the target halted, and return why it stopped.
    pub fn step(&mut self, kind: StepKind) -> Result<StopReason> {
        if *self.state() != RunState::Halted {
            return Err(EngineError::Other("the target is running".to_owned()));
        }
        // Stepping into an inlined body shown as a call needs no execution.
        if kind == StepKind::Into && self.hidden_inline > 0 {
            self.hidden_inline -= 1;
            self.invalidate();
            return Ok(StopReason::Step);
        }
        self.cancel.store(false, Ordering::SeqCst);
        // Background reads would otherwise compete with each step for USB.
        let _background = self.handle.suspend_background();
        let mask = self.debug_config().mask_interrupts_on_step;
        if mask {
            self.set_mask_interrupts(true)?;
        }
        let outcome = match kind {
            StepKind::Out => self.step_out(),
            _ => self.step_line(kind == StepKind::Into),
        };
        if mask && *self.state() == RunState::Halted {
            self.set_mask_interrupts(false)?;
        }
        let reason = outcome?;
        self.state = RunState::Halted;
        if reason != StopReason::Step {
            self.stop_location = None;
        }
        self.invalidate();
        self.note_stop(&reason)?;
        Ok(reason)
    }

    fn step_out(&mut self) -> Result<StopReason> {
        let start = self.frame_state()?;
        let chain = self.db.scope_at(start.pc)?;
        let visible = chain.len().saturating_sub(self.hidden_inline);
        self.begin_stop_tracking();

        if visible >= 2 {
            // Leaving an inlined instance: step until its range is left.
            let instance = chain[visible - 1].die;
            let Some(function) = self.db.function_at(start.pc) else {
                return Ok(StopReason::Step);
            };
            let targets = self.exit_addresses(
                function.low,
                function.high,
                start.return_address,
                |scope, _| !scope.iter().any(|frame| frame.die == instance),
            )?;
            let mut steps = 0;
            let mut skip_step = false;
            loop {
                if self.cancelled() {
                    return Ok(StopReason::Pause);
                }
                if !skip_step && steps >= SINGLE_STEPS_BEFORE_ESCAPE {
                    if let Some(kind) = self.escape_kind(targets.len(), steps) {
                        let floor = start.cfa;
                        match self.run_to(&targets, kind, move |state| state.cfa >= floor)? {
                            RunOutcome::Reached => skip_step = true,
                            RunOutcome::Stopped(reason) => return Ok(reason),
                        }
                    }
                }
                if !skip_step {
                    self.single_step()?;
                    steps += 1;
                }
                skip_step = false;
                let state = self.frame_state()?;
                if state.cfa < start.cfa {
                    match self.finish_call(state.return_address, state.cfa.max(start.cfa))? {
                        RunOutcome::Reached => {}
                        RunOutcome::Stopped(reason) => return Ok(reason),
                    }
                    continue;
                }
                let now = chain_ids(&self.db.scope_at(state.pc)?);
                if !now.contains(&instance) || state.cfa > start.cfa || steps >= MAX_SINGLE_STEPS {
                    return Ok(StopReason::Step);
                }
            }
        }

        match self.finish_call(start.return_address, start.cfa)? {
            RunOutcome::Reached => Ok(StopReason::Step),
            RunOutcome::Stopped(reason) => Ok(reason),
        }
    }

    fn step_line(&mut self, into: bool) -> Result<StopReason> {
        let mut origin = self.frame_state()?;
        let start_function = self.db.function_at(origin.pc);
        let chain = self.db.scope_at(origin.pc)?;
        let mut depth = chain.len().saturating_sub(self.hidden_inline);
        let mut identity = chain_ids(&chain[..depth.min(chain.len())]);
        let mut line = self.visible_location(origin.pc, &chain)?;
        self.begin_stop_tracking();

        let Some(function) = start_function else {
            // No line info here: behave like step out.
            return match self.finish_call(origin.return_address, origin.cfa)? {
                RunOutcome::Reached => Ok(StopReason::Step),
                RunOutcome::Stopped(reason) => Ok(reason),
            };
        };
        let (mut low, mut high) = (function.low, function.high);
        let mut current_low = function.low;

        let mut steps_here = 0u32;
        let mut total = 0u32;
        let mut escape_stage = 0u32;
        let mut skip_step = false;
        loop {
            if self.cancelled() {
                return Ok(StopReason::Pause);
            }
            if total >= MAX_SINGLE_STEPS {
                return Ok(StopReason::Step);
            }
            let threshold = match escape_stage {
                0 => SINGLE_STEPS_BEFORE_ESCAPE,
                1 => SINGLE_STEPS_BEFORE_FLASH_ESCAPE,
                _ => u32::MAX,
            };
            if !skip_step && steps_here >= threshold {
                escape_stage += 1;
                let depth_limit = depth;
                let current_line = line.clone();
                let targets =
                    self.exit_addresses(low, high, origin.return_address, |scope, at| {
                        scope.len() <= depth_limit && Some(at) != current_line.as_ref()
                    })?;
                if let Some(kind) = self.escape_kind(targets.len(), steps_here) {
                    let floor = origin.cfa;
                    match self.run_to(&targets, kind, move |state| state.cfa >= floor)? {
                        RunOutcome::Reached => skip_step = true,
                        RunOutcome::Stopped(reason) => return Ok(reason),
                    }
                }
            }
            if !skip_step {
                self.single_step()?;
                steps_here += 1;
                total += 1;
            }
            skip_step = false;

            let state = self.frame_state()?;
            let here = self.db.function_at(state.pc);
            let entered = here
                .as_ref()
                .is_some_and(|f| f.low == state.pc && f.low != current_low);
            let no_debug_info = here.is_none();

            // A call, or a tail call that left this frame's depth unchanged.
            if state.cfa < origin.cfa || entered || (no_debug_info && state.cfa <= origin.cfa) {
                let has_lines = here.is_some() && self.db.line_at(state.pc)?.is_some();
                if into && has_lines {
                    return self.finish_step_into(&state);
                }
                match self.finish_call(state.return_address, state.cfa.max(origin.cfa))? {
                    RunOutcome::Reached => {}
                    RunOutcome::Stopped(reason) => return Ok(reason),
                }
                skip_step = true;
                continue;
            }

            let scope = self.db.scope_at(state.pc)?;
            let ids = chain_ids(&scope);

            if state.cfa > origin.cfa {
                // Returned to the caller, usually mid-line. Keep going to the
                // next statement there, treating the caller as the new origin.
                origin = state.clone();
                if let Some(function) = &here {
                    low = function.low;
                    high = function.high;
                    current_low = function.low;
                }
                depth = scope.len();
                identity = ids.clone();
                line = None;
                steps_here = 0;
                escape_stage = 0;
            }

            if prefix_of(&identity, &ids) && ids.len() > depth {
                // Inside an inlined callee of this line.
                if into {
                    if scope.last().is_some_and(|frame| frame.entry_pc == state.pc) {
                        return Ok(StopReason::Step);
                    }
                } else {
                    // A new inlined call on another line is a stop in its own
                    // right, presented at the call site.
                    let callee = &scope[depth];
                    let starts_here = callee.entry_pc == state.pc;
                    if starts_here && callee.call_site.is_some() && callee.call_site != line {
                        return Ok(StopReason::Step);
                    }
                    continue;
                }
            }

            if self.db.is_statement_start(state.pc)? {
                let location = self.db.line_at(state.pc)?;
                if location.is_some() && location != line {
                    return Ok(StopReason::Step);
                }
            }
        }
    }

    /// Entered a function with line info: stop past its prologue.
    fn finish_step_into(&mut self, state: &FrameState) -> Result<StopReason> {
        let Some(function) = self.db.function_at(state.pc) else {
            return Ok(StopReason::Step);
        };
        let target = self.db.after_prologue(function.low, function.high)?;
        if target == state.pc {
            return Ok(StopReason::Step);
        }
        match self.run_to(&[target], BpKind::Hw, |_| true)? {
            RunOutcome::Reached => Ok(StopReason::Step),
            RunOutcome::Stopped(reason) => Ok(reason),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefix_matching_compares_leading_ids() {
        assert!(prefix_of(&[1, 2], &[1, 2, 3]));
        assert!(prefix_of(&[1, 2], &[1, 2]));
        assert!(!prefix_of(&[1, 2], &[1, 3, 2]));
        assert!(!prefix_of(&[1, 2, 3], &[1, 2]));
    }
}
