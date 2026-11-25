//! Stack unwinding from `.debug_frame`, with Thumb prologue fallback and
//! Cortex-M exception frame / PSP heuristic recovery (DESIGN §5.5.4).

use gimli::{BaseAddresses, CfaRule, RegisterRule, UnwindContext, UnwindSection};

use crate::db::SymbolDb;
use crate::index::R;
use crate::{MemoryReader, Result, SymbolError};

const R12: usize = 12;
const SP: usize = 13;
const MAX_FRAMES: usize = 64;
const MAX_PROLOGUE_BYTES: u32 = 512;
const LR: usize = 14;
const PC: usize = 15;

const EXC_RETURN_USES_PSP: u32 = 1 << 2;
const EXC_RETURN_BASIC_FRAME: u32 = 1 << 4;
const EXC_RETURN_BASIC_THREAD_PSP: u32 = 0xFFFF_FFFD;
const EXC_RETURN_EXT_THREAD_PSP: u32 = 0xFFFF_FFED;
const XPSR_STACK_ALIGNED: u32 = 1 << 9;
const XPSR_THUMB: u32 = 1 << 24;
const XPSR_EXCEPTION_MASK: u32 = 0x1FF;
const BASIC_FRAME_BYTES: u32 = 8 * 4;
/// Basic frame plus S0-S15, FPSCR and a reserved word.
const EXTENDED_FRAME_BYTES: u32 = BASIC_FRAME_BYTES + 18 * 4;

const ICSR: u32 = 0xE000_ED04;
const ICSR_RETTOBASE: u32 = 1 << 11;
const FPCCR: u32 = 0xE000_EF34;
const FPCCR_LSPACT: u32 = 1 << 0;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CoreRegisters {
    pub r: [u32; 16],
    pub xpsr: u32,
    pub msp: u32,
    pub psp: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrameKind {
    Code,
    /// Boundary between a handler and the code it interrupted.
    Exception {
        number: u32,
        heuristic: bool,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UnwindStopReason {
    Bottom,
    LimitReached,
    ZeroReturnAddress { pc: u32 },
    NoUnwindInfo { pc: u32 },
    MemoryError { pc: u32, addr: u32 },
    CfaDidNotAdvance { pc: u32, cfa: u32 },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StackFrame {
    pub kind: FrameKind,
    pub pc: u32,
    pub sp: u32,
    pub function: Option<String>,
    /// Address to use for symbol lookups: inside the call for return addresses.
    pub lookup_pc: u32,
    /// Canonical frame address, when CFI or prologue analysis covers this frame.
    pub cfa: Option<u32>,
    pub registers: [Option<u32>; 16],
}

type Registers = [Option<u32>; 16];

fn read_word(memory: &mut dyn MemoryReader, addr: u32) -> Result<u32> {
    let mut bytes = [0u8; 4];
    memory.read(addr, &mut bytes).map_err(SymbolError::Memory)?;
    Ok(u32::from_le_bytes(bytes))
}

fn is_exc_return(value: u32) -> bool {
    value >= 0xFFFF_FF00 && value != u32::MAX
}

/// Bytes the hardware pushed for an exception.
pub fn exception_frame_size(exc_return: u32, stacked_xpsr: u32) -> u32 {
    let base = if exc_return & EXC_RETURN_BASIC_FRAME != 0 {
        BASIC_FRAME_BYTES
    } else {
        EXTENDED_FRAME_BYTES
    };
    base + if stacked_xpsr & XPSR_STACK_ALIGNED != 0 {
        4
    } else {
        0
    }
}

fn thumb_expand_imm(imm12: u32) -> u32 {
    let top2 = (imm12 >> 10) & 0x3;
    if top2 == 0 {
        let imm8 = imm12 & 0xFF;
        match (imm12 >> 8) & 0x3 {
            0 => imm8,
            1 => (imm8 << 16) | imm8,
            2 => (imm8 << 24) | (imm8 << 8),
            _ => (imm8 << 24) | (imm8 << 16) | (imm8 << 8) | imm8,
        }
    } else {
        let unrotated = 0x80 | (imm12 & 0x7F);
        let rot = (imm12 >> 7) & 0x1F;
        unrotated.rotate_right(rot)
    }
}

fn is_thumb_32(hw1: u16) -> bool {
    matches!(hw1 >> 11, 0b11101..=0b11111)
}

fn apply_push_mask(sp_offset: &mut u32, reg_offset: &mut [Option<i32>; 16], mask: u32) {
    for reg in (0..15).rev() {
        if (mask & (1 << reg)) != 0 {
            *sp_offset = sp_offset.saturating_add(4);
            reg_offset[reg] = Some(-(*sp_offset as i32));
        }
    }
}

fn apply_pop_mask(sp_offset: &mut u32, reg_offset: &mut [Option<i32>; 16], mask: u32) {
    for (reg, slot) in reg_offset.iter_mut().enumerate().take(15) {
        if (mask & (1 << reg)) != 0 {
            *slot = None;
            *sp_offset = sp_offset.saturating_sub(4);
        }
    }
}

/// Scan Thumb-1 / Thumb-2 instructions from function entry up to `bytes.len()`
/// to infer stack adjustment and saved register slots when `.debug_frame` has no
/// CFI entry (for instance handwritten RTOS / startup assembly).
fn analyze_thumb_bytes(
    bytes: &[u8],
    sp: u32,
    registers: &Registers,
    pc_is_return_address: bool,
    memory: &mut dyn MemoryReader,
) -> std::result::Result<Option<(u32, Registers)>, u32> {
    if !bytes.len().is_multiple_of(2) {
        return Ok(None);
    }
    let mut sp_offset: u32 = 0;
    let mut reg_offset: [Option<i32>; 16] = [None; 16];
    let mut it_remaining: u8 = 0;
    let mut offset = 0usize;

    while offset + 2 <= bytes.len() {
        let hw1 = u16::from_le_bytes([bytes[offset], bytes[offset + 1]]);
        if is_thumb_32(hw1) {
            if offset + 4 > bytes.len() {
                break;
            }
            let hw2 = u16::from_le_bytes([bytes[offset + 2], bytes[offset + 3]]);
            offset += 4;
            if it_remaining > 0 {
                it_remaining -= 1;
                continue;
            }
            if hw1 == 0xE92D {
                // STMDB SP!, <reglist> / PUSH.W <reglist>
                apply_push_mask(&mut sp_offset, &mut reg_offset, u32::from(hw2) & 0x5FFF);
            } else if hw1 == 0xF84D && (hw2 & 0x0FFF) == 0x0D04 {
                // STR.W Rt, [SP, #-4]! / PUSH.W {Rt}
                let rt = ((hw2 >> 12) & 0xF) as usize;
                sp_offset = sp_offset.saturating_add(4);
                if rt < 15 {
                    reg_offset[rt] = Some(-(sp_offset as i32));
                }
            } else if hw1 == 0xE8BD {
                // LDMIA.W SP!, <reglist> / POP.W <reglist>
                if (hw2 & (1 << 15)) != 0 {
                    break;
                }
                apply_pop_mask(&mut sp_offset, &mut reg_offset, u32::from(hw2) & 0x5FFF);
            } else if (hw1 & 0xFFBF) == 0xED2D && (hw2 & 0x0E00) == 0x0A00 {
                // VPUSH {Sd..} / VPUSH {Dd..}
                sp_offset = sp_offset.saturating_add(u32::from(hw2 & 0x00FF) * 4);
            } else if (hw1 & 0xFBFF) == 0xF1AD && (hw2 & 0x8F00) == 0x0D00 {
                // SUB.W SP, SP, #const
                let imm12 = (u32::from((hw1 >> 10) & 1) << 11)
                    | (u32::from((hw2 >> 12) & 7) << 8)
                    | u32::from(hw2 & 0xFF);
                sp_offset = sp_offset.saturating_add(thumb_expand_imm(imm12));
            } else if (hw1 & 0xFBFF) == 0xF2AD && (hw2 & 0x8F00) == 0x0D00 {
                // SUBW SP, SP, #imm12
                let imm12 = (u32::from((hw1 >> 10) & 1) << 11)
                    | (u32::from((hw2 >> 12) & 7) << 8)
                    | u32::from(hw2 & 0xFF);
                sp_offset = sp_offset.saturating_add(imm12);
            } else if (hw1 & 0xF800) == 0xF000 && (hw2 & 0xD000) == 0x9000 {
                // Unconditional B.W
                break;
            }
        } else {
            offset += 2;
            if it_remaining > 0 {
                it_remaining -= 1;
                continue;
            }
            if (hw1 & 0xFF00) == 0xBF00 && (hw1 & 0x000F) != 0 {
                // IT{x{y{z}}}
                it_remaining = (4 - (hw1 & 0x000F).trailing_zeros()) as u8;
            } else if (hw1 & 0xFE00) == 0xB400 {
                // PUSH {reglist, lr}
                let mask = u32::from(hw1 & 0x00FF) | if hw1 & 0x0100 != 0 { 1 << LR } else { 0 };
                apply_push_mask(&mut sp_offset, &mut reg_offset, mask);
            } else if (hw1 & 0xFF80) == 0xB080 {
                // SUB SP, SP, #imm7
                sp_offset = sp_offset.saturating_add(u32::from(hw1 & 0x007F) * 4);
            } else if (hw1 & 0xFF80) == 0xB000 {
                // ADD SP, SP, #imm7
                sp_offset = sp_offset.saturating_sub(u32::from(hw1 & 0x007F) * 4);
            } else if (hw1 & 0xFE00) == 0xBC00 {
                // POP {reglist, pc}
                if (hw1 & 0x0100) != 0 {
                    break;
                }
                apply_pop_mask(&mut sp_offset, &mut reg_offset, u32::from(hw1 & 0x00FF));
            } else if (hw1 & 0xFF80) == 0x4700 || (hw1 & 0xF800) == 0xE000 {
                // BX Rm or unconditional B
                break;
            }
        }
    }

    if sp_offset == 0 && reg_offset[LR].is_none() {
        return Ok(None);
    }

    let cfa = sp.wrapping_add(sp_offset);
    let mut caller = *registers;
    caller[SP] = Some(cfa);
    if pc_is_return_address {
        caller[LR] = None;
    }
    for (reg, slot) in reg_offset.iter().enumerate() {
        if let Some(rel) = slot {
            let addr = cfa.wrapping_add(*rel as u32);
            let value = read_word(memory, addr).map_err(|_| addr)?;
            caller[reg] = Some(value);
        }
    }
    Ok(Some((cfa, caller)))
}

fn is_plausible_ram_sp(addr: u32) -> bool {
    addr != 0 && addr.is_multiple_of(4) && (0x1000_0000..0xE000_0000).contains(&addr)
}

/// Read the 8-word Cortex-M hardware exception frame at `psp`, plus any
/// FreeRTOS-saved `r4..=r11` + `EXC_RETURN` tucked immediately below `psp`.
fn try_recover_psp_exception(
    memory: &mut dyn MemoryReader,
    xpsr: u32,
    current_sp: u32,
    psp: u32,
    crossed_exception: bool,
    registers: &Registers,
) -> Option<(StackFrame, Registers, u32)> {
    let exc_num = xpsr & XPSR_EXCEPTION_MASK;
    if exc_num == 0 || crossed_exception || !is_plausible_ram_sp(psp) {
        return None;
    }
    if current_sp.abs_diff(psp) < 16 {
        return None;
    }
    if let Ok(icsr) = read_word(memory, ICSR) {
        if (icsr & ICSR_RETTOBASE) == 0 {
            return None;
        }
    }
    let mut saved = [0u32; 8];
    for (slot, word) in saved.iter_mut().enumerate() {
        *word = read_word(memory, psp.wrapping_add(slot as u32 * 4)).ok()?;
    }
    let saved_pc = saved[6] & !1;
    let saved_xpsr = saved[7];
    if (saved_xpsr & XPSR_THUMB) == 0
        || (saved_xpsr & XPSR_EXCEPTION_MASK) != 0
        || saved_pc == 0
        || saved_pc >= 0xE000_0000
    {
        return None;
    }

    // Check whether FreeRTOS PendSV saved EXC_RETURN + r4..=r11 below `psp`.
    let mut recovered_callee_saved: Option<[u32; 8]> = None;
    let exc_return =
        if read_word(memory, psp.wrapping_sub(4)).is_ok_and(|w| w == EXC_RETURN_BASIC_THREAD_PSP) {
            let mut r4_11 = [0u32; 8];
            if (0..8).all(|i| {
                read_word(memory, psp.wrapping_sub(9 * 4 - i as u32 * 4))
                    .map(|v| r4_11[i] = v)
                    .is_ok()
            }) {
                recovered_callee_saved = Some(r4_11);
            }
            EXC_RETURN_BASIC_THREAD_PSP
        } else if read_word(memory, psp.wrapping_sub(17 * 4))
            .is_ok_and(|w| w == EXC_RETURN_EXT_THREAD_PSP)
        {
            let mut r4_11 = [0u32; 8];
            if (0..8).all(|i| {
                read_word(memory, psp.wrapping_sub(25 * 4 - i as u32 * 4))
                    .map(|v| r4_11[i] = v)
                    .is_ok()
            }) {
                recovered_callee_saved = Some(r4_11);
            }
            EXC_RETURN_EXT_THREAD_PSP
        } else if read_word(memory, FPCCR).is_ok_and(|fpccr| (fpccr & FPCCR_LSPACT) != 0) {
            EXC_RETURN_EXT_THREAD_PSP
        } else {
            EXC_RETURN_BASIC_THREAD_PSP
        };

    let mut caller = *registers;
    if let Some(r4_11) = recovered_callee_saved {
        for (i, val) in r4_11.into_iter().enumerate() {
            caller[4 + i] = Some(val);
        }
    }
    for (index, value) in saved[..4].iter().enumerate() {
        caller[index] = Some(*value);
    }
    caller[R12] = Some(saved[4]);
    caller[LR] = Some(saved[5]);
    caller[PC] = Some(saved[6]);
    caller[SP] = Some(psp.wrapping_add(exception_frame_size(exc_return, saved_xpsr)));

    let frame = StackFrame {
        kind: FrameKind::Exception {
            number: exc_num,
            heuristic: true,
        },
        pc: exc_return,
        sp: psp,
        function: None,
        lookup_pc: exc_return,
        cfa: None,
        registers: *registers,
    };
    Some((frame, caller, saved_xpsr))
}

impl SymbolDb {
    /// Function or, for assembly without debug info, ELF symbol covering `pc`.
    pub fn name_at(&self, pc: u32) -> Option<String> {
        self.function_at(pc)
            .map(|function| function.name)
            .or_else(|| self.symbol_at(pc))
    }

    /// CFA and caller registers from CFI, or None when `pc` has none.
    fn apply_cfi(
        &self,
        context: &mut UnwindContext<usize>,
        pc: u32,
        registers: &Registers,
        pc_is_return_address: bool,
        memory: &mut dyn MemoryReader,
    ) -> std::result::Result<Option<(u32, Registers)>, SymbolError> {
        let bases = BaseAddresses::default();
        let found = self.debug_frame.fde_for_address(
            &bases,
            u64::from(pc),
            gimli::DebugFrame::<R>::cie_from_offset,
        );
        let fde = match found {
            Ok(fde) => fde,
            Err(gimli::Error::NoUnwindInfoForAddress) => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let row = fde.unwind_info_for_address(&self.debug_frame, &bases, context, u64::from(pc))?;
        let CfaRule::RegisterAndOffset { register, offset } = row.cfa() else {
            return Ok(None);
        };
        // GCC emits empty FDEs for __attribute__((naked)) / inline-asm functions
        // (CFA = sp + 0 and no saved-register rules). Let the Thumb prologue
        // scanner recover the real stack offset and saved registers inside them.
        if (register.0 as usize) == SP
            && *offset == 0
            && u64::from(pc) > fde.initial_address()
            && !row.registers().any(|_| true)
        {
            return Ok(None);
        }
        let Some(base) = registers.get(register.0 as usize).copied().flatten() else {
            return Ok(None);
        };
        let cfa = base.wrapping_add(*offset as u32);

        let mut caller = *registers;
        caller[SP] = Some(cfa);
        if pc_is_return_address {
            caller[LR] = None;
        }
        for (register, rule) in row.registers() {
            let index = register.0 as usize;
            if index >= caller.len() {
                continue;
            }
            caller[index] = match rule {
                RegisterRule::Undefined => None,
                RegisterRule::SameValue => registers[index],
                RegisterRule::Offset(offset) => {
                    Some(read_word(memory, cfa.wrapping_add(*offset as u32))?)
                }
                RegisterRule::ValOffset(offset) => Some(cfa.wrapping_add(*offset as u32)),
                RegisterRule::Register(source) => {
                    registers.get(source.0 as usize).copied().flatten()
                }
                _ => None,
            };
        }
        Ok(Some((cfa, caller)))
    }

    fn analyze_prologue(
        &self,
        lookup: u32,
        target_pc: u32,
        sp: u32,
        registers: &Registers,
        pc_is_return_address: bool,
        memory: &mut dyn MemoryReader,
    ) -> std::result::Result<Option<(u32, Registers)>, u32> {
        let Some(start_pc) = self.function_start_at(lookup) else {
            return Ok(None);
        };
        let start_pc = start_pc & !1;
        let end_pc = target_pc & !1;
        if end_pc <= start_pc || end_pc - start_pc > MAX_PROLOGUE_BYTES {
            return Ok(None);
        }
        let mut bytes = vec![0u8; (end_pc - start_pc) as usize];
        if self.read_code(start_pc, &mut bytes, memory).is_err() {
            return Ok(None);
        }
        analyze_thumb_bytes(&bytes, sp, registers, pc_is_return_address, memory)
    }

    /// CFA and return address of the innermost frame. Code without CFI is
    /// treated as a leaf: nothing pushed, return address in LR.
    pub fn frame_info(
        &self,
        registers: &CoreRegisters,
        memory: &mut dyn MemoryReader,
    ) -> Result<(u32, u32)> {
        let current: Registers = registers.r.map(Some);
        let mut context = UnwindContext::new();
        let pc = registers.r[PC] & !1;
        let unwound = self.apply_cfi(&mut context, pc, &current, false, memory)?;
        Ok(match unwound {
            Some((cfa, caller)) => (cfa, caller[LR].unwrap_or(0)),
            None => (registers.r[SP], registers.r[LR]),
        })
    }

    /// Walk the stack from `start`. A memory error ends the walk and returns
    /// the frames found so far, since a partial stack is still useful.
    pub fn unwind(
        &self,
        start: &CoreRegisters,
        memory: &mut dyn MemoryReader,
    ) -> Result<Vec<StackFrame>> {
        self.unwind_limited(start, memory, MAX_FRAMES)
    }

    /// Like [`Self::unwind`], stopping after `limit` frames.
    pub fn unwind_limited(
        &self,
        start: &CoreRegisters,
        memory: &mut dyn MemoryReader,
        limit: usize,
    ) -> Result<Vec<StackFrame>> {
        Ok(self.unwind_detailed(start, memory, limit)?.0)
    }

    /// Walk the stack from `start`, also returning why unwinding ended.
    pub fn unwind_detailed(
        &self,
        start: &CoreRegisters,
        memory: &mut dyn MemoryReader,
        limit: usize,
    ) -> Result<(Vec<StackFrame>, UnwindStopReason)> {
        let mut registers: Registers = start.r.map(Some);
        let mut xpsr = start.xpsr;
        let mut context = UnwindContext::new();
        let mut frames = Vec::new();
        let mut previous_cfa: Option<u32> = None;
        // A return address points after the call, possibly past the function.
        let mut pc_is_return_address = false;
        let mut crossed_exception = false;
        let mut stop_reason = UnwindStopReason::LimitReached;

        while frames.len() < limit {
            let Some(pc) = registers[PC].map(|pc| pc & !1).filter(|pc| *pc != 0) else {
                stop_reason = UnwindStopReason::ZeroReturnAddress {
                    pc: frames.last().map_or(0, |f: &StackFrame| f.pc),
                };
                break;
            };
            let Some(sp) = registers[SP] else {
                stop_reason = UnwindStopReason::NoUnwindInfo { pc };
                break;
            };
            let mut lookup = if pc_is_return_address { pc - 1 } else { pc };
            // FreeRTOS plants this function's own address as the return address
            // of task entry points, so it must be matched without the -1.
            let at_pc = self.name_at(pc);
            let stop_here = matches!(
                at_pc.as_deref(),
                Some("Reset_Handler" | "prvTaskExitError" | "vPortStartFirstTask")
            );
            if stop_here {
                lookup = pc;
            }
            let function = if pc_is_return_address && !stop_here {
                self.name_at(lookup)
            } else {
                at_pc
            };
            frames.push(StackFrame {
                kind: FrameKind::Code,
                pc,
                sp,
                function: function.clone(),
                lookup_pc: lookup,
                cfa: None,
                registers,
            });
            if stop_here {
                stop_reason = UnwindStopReason::Bottom;
                break;
            }

            let cfi_result = match self.apply_cfi(
                &mut context,
                lookup,
                &registers,
                pc_is_return_address,
                memory,
            ) {
                Ok(unwound) => unwound,
                Err(SymbolError::Memory(_)) => {
                    stop_reason = UnwindStopReason::MemoryError { pc, addr: sp };
                    break;
                }
                Err(error) => return Err(error),
            };

            let (cfa, mut caller, used_unwind_info) = match cfi_result {
                Some((cfa, caller)) => {
                    if let Some(frame) = frames.last_mut() {
                        frame.cfa = Some(cfa);
                    }
                    (cfa, caller, true)
                }
                None => match self.analyze_prologue(
                    lookup,
                    pc,
                    sp,
                    &registers,
                    pc_is_return_address,
                    memory,
                ) {
                    Ok(Some((cfa, caller))) => {
                        if let Some(frame) = frames.last_mut() {
                            frame.cfa = Some(cfa);
                        }
                        (cfa, caller, true)
                    }
                    Ok(None) if !pc_is_return_address => (sp, registers, false),
                    Ok(None) => (sp, [None; 16], false),
                    Err(addr) => {
                        stop_reason = UnwindStopReason::MemoryError { pc, addr };
                        break;
                    }
                },
            };

            let maybe_lr = caller[LR];
            if let Some(return_address) = maybe_lr {
                if is_exc_return(return_address) {
                    let stacked_at = if return_address & EXC_RETURN_USES_PSP != 0 {
                        start.psp
                    } else {
                        cfa
                    };
                    let mut saved = [0u32; 8];
                    let mut bad_addr = None;
                    for (slot, word) in saved.iter_mut().enumerate() {
                        let addr = stacked_at.wrapping_add(slot as u32 * 4);
                        if let Ok(value) = read_word(memory, addr) {
                            *word = value;
                        } else {
                            bad_addr = Some(addr);
                            break;
                        }
                    }
                    if let Some(addr) = bad_addr {
                        stop_reason = UnwindStopReason::MemoryError { pc, addr };
                        break;
                    }
                    frames.push(StackFrame {
                        kind: FrameKind::Exception {
                            number: xpsr & XPSR_EXCEPTION_MASK,
                            heuristic: false,
                        },
                        pc: return_address,
                        sp: stacked_at,
                        function: None,
                        lookup_pc: return_address,
                        cfa: None,
                        registers,
                    });
                    for (index, value) in saved[..4].iter().enumerate() {
                        caller[index] = Some(*value);
                    }
                    caller[R12] = Some(saved[4]);
                    caller[LR] = Some(saved[5]);
                    caller[PC] = Some(saved[6]);
                    xpsr = saved[7];
                    caller[SP] =
                        Some(stacked_at.wrapping_add(exception_frame_size(return_address, xpsr)));
                    registers = caller;
                    previous_cfa = None;
                    pc_is_return_address = false;
                    crossed_exception = true;
                    continue;
                }

                if return_address == 0 || return_address == u32::MAX {
                    stop_reason = if function.as_deref() == Some("main") {
                        UnwindStopReason::Bottom
                    } else {
                        UnwindStopReason::ZeroReturnAddress { pc }
                    };
                    break;
                }
                if !pc_is_return_address && (return_address & !1) == pc {
                    stop_reason = if function.as_deref() == Some("main") {
                        UnwindStopReason::Bottom
                    } else {
                        UnwindStopReason::NoUnwindInfo { pc }
                    };
                    break;
                }
                if previous_cfa.is_some_and(|previous| cfa <= previous) {
                    stop_reason = UnwindStopReason::CfaDidNotAdvance { pc, cfa };
                    break;
                }
                caller[PC] = Some(return_address);
                registers = caller;
                previous_cfa = Some(cfa);
                pc_is_return_address = true;
                continue;
            }

            if let Some((exc_frame, exc_caller, next_xpsr)) = try_recover_psp_exception(
                memory,
                xpsr,
                sp,
                start.psp,
                crossed_exception,
                &registers,
            ) {
                frames.push(exc_frame);
                registers = exc_caller;
                xpsr = next_xpsr;
                previous_cfa = None;
                pc_is_return_address = false;
                crossed_exception = true;
                continue;
            }

            stop_reason = if used_unwind_info {
                UnwindStopReason::ZeroReturnAddress { pc }
            } else {
                UnwindStopReason::NoUnwindInfo { pc }
            };
            break;
        }
        Ok((frames, stop_reason))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[derive(Default)]
    struct MapMem(BTreeMap<u32, u32>);

    impl MapMem {
        fn write_words(&mut self, base: u32, words: &[u32]) {
            for (i, w) in words.iter().enumerate() {
                self.0.insert(base + i as u32 * 4, *w);
            }
        }
    }

    impl MemoryReader for MapMem {
        fn read(&mut self, addr: u32, buf: &mut [u8]) -> std::result::Result<(), String> {
            if !addr.is_multiple_of(4) || buf.len() != 4 {
                return Err(format!("unaligned read at {addr:#x}"));
            }
            let word = self
                .0
                .get(&addr)
                .copied()
                .ok_or_else(|| format!("unmapped {addr:#x}"))?;
            buf.copy_from_slice(&word.to_le_bytes());
            Ok(())
        }
    }

    #[test]
    fn frame_size_accounts_for_fpu_and_alignment() {
        assert_eq!(exception_frame_size(0xFFFF_FFFD, 0), 0x20);
        assert_eq!(exception_frame_size(0xFFFF_FFFD, 1 << 9), 0x24);
        assert_eq!(exception_frame_size(0xFFFF_FFED, 0), 0x68);
        assert_eq!(exception_frame_size(0xFFFF_FFED, 1 << 9), 0x6C);
    }

    #[test]
    fn exc_return_is_recognized() {
        assert!(is_exc_return(0xFFFF_FFF9));
        assert!(is_exc_return(0xFFFF_FFED));
        assert!(!is_exc_return(u32::MAX));
        assert!(!is_exc_return(0x0800_1234));
    }

    #[test]
    fn analyzes_thumb_prologues_and_skips_it_blocks() {
        let mut mem = MapMem::default();
        let sp = 0x2000_1000;
        // Stack contents after:
        //   push {r4, lr}          -> [sp+16]=r4, [sp+20]=lr
        //   sub sp, #16            -> 4 words local
        //   stmdb sp!, {r5, r6}    -> [sp+0]=r5, [sp+4]=r6
        mem.write_words(sp, &[55, 66, 0, 0, 0, 0, 44, 0x0800_2221]);

        let code_halfwords: Vec<u16> = vec![
            0xB510, // push {r4, lr}
            0xB084, // sub sp, #16
            0xBF08, // it eq (1 instruction follows)
            0xB084, // subeq sp, #16 (skipped because inside IT block!)
            0xE92D, 0x0060, // push.w {r5, r6}
        ];
        let bytes: Vec<u8> = code_halfwords
            .into_iter()
            .flat_map(u16::to_le_bytes)
            .collect();

        let regs = [Some(0); 16];
        let (cfa, caller) = analyze_thumb_bytes(&bytes, sp, &regs, true, &mut mem)
            .unwrap()
            .unwrap();
        assert_eq!(cfa, sp + 32);
        assert_eq!(caller[4], Some(44));
        assert_eq!(caller[5], Some(55));
        assert_eq!(caller[6], Some(66));
        assert_eq!(caller[LR], Some(0x0800_2221));
    }

    #[test]
    fn recovers_psp_exception_boundary_heuristically() {
        let mut mem = MapMem::default();
        let psp = 0x2000_4000;
        let msp = 0x2000_0800;
        // Write FreeRTOS-style r4-r11 + EXC_RETURN below PSP
        let mut below: Vec<u32> = (104..=111).collect();
        below.push(EXC_RETURN_BASIC_THREAD_PSP);
        mem.write_words(psp - 9 * 4, &below);
        // Hardware exception frame at PSP
        mem.write_words(
            psp,
            &[10, 11, 12, 13, 112, 0x0800_3001, 0x0800_2004, 0x0100_0000],
        );
        mem.write_words(ICSR, &[ICSR_RETTOBASE]);

        let mut regs = [Some(0); 16];
        regs[SP] = Some(msp);
        let (frame, caller, next_xpsr) =
            try_recover_psp_exception(&mut mem, 14, msp, psp, false, &regs).unwrap();
        assert_eq!(
            frame.kind,
            FrameKind::Exception {
                number: 14,
                heuristic: true
            }
        );
        assert_eq!(caller[4], Some(104));
        assert_eq!(caller[11], Some(111));
        assert_eq!(caller[PC], Some(0x0800_2004));
        assert_eq!(caller[LR], Some(0x0800_3001));
        assert_eq!(caller[SP], Some(psp + 0x20));
        assert_eq!(next_xpsr, 0x0100_0000);
    }
}
