//! Stack unwinding from `.debug_frame`, including Cortex-M exception frames
//! (DESIGN §5.5.4).

use gimli::{BaseAddresses, CfaRule, RegisterRule, UnwindContext, UnwindSection};

use crate::db::SymbolDb;
use crate::index::R;
use crate::{MemoryReader, Result, SymbolError};

const R12: usize = 12;
const SP: usize = 13;
const LR: usize = 14;
const PC: usize = 15;
const MAX_FRAMES: usize = 64;

const EXC_RETURN_USES_PSP: u32 = 1 << 2;
const EXC_RETURN_BASIC_FRAME: u32 = 1 << 4;
const XPSR_STACK_ALIGNED: u32 = 1 << 9;
const XPSR_EXCEPTION_MASK: u32 = 0x1FF;
const BASIC_FRAME_BYTES: u32 = 8 * 4;
/// Basic frame plus S0-S15, FPSCR and a reserved word.
const EXTENDED_FRAME_BYTES: u32 = BASIC_FRAME_BYTES + 18 * 4;

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
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StackFrame {
    pub kind: FrameKind,
    pub pc: u32,
    pub sp: u32,
    pub function: Option<String>,
    /// Address to use for symbol lookups: inside the call for return addresses.
    pub lookup_pc: u32,
    /// Canonical frame address, when CFI covers this frame.
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
        memory: &mut dyn MemoryReader,
    ) -> Result<Option<(u32, Registers)>> {
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
        let Some(base) = registers.get(register.0 as usize).copied().flatten() else {
            return Ok(None);
        };
        let cfa = base.wrapping_add(*offset as u32);

        let mut caller = *registers;
        caller[SP] = Some(cfa);
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
        let unwound = self.apply_cfi(&mut context, pc, &current, memory)?;
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
        let mut registers: Registers = start.r.map(Some);
        let mut xpsr = start.xpsr;
        let mut context = UnwindContext::new();
        let mut frames = Vec::new();
        let mut previous_cfa: Option<u32> = None;
        // A return address points after the call, possibly past the function.
        let mut pc_is_return_address = false;

        while frames.len() < MAX_FRAMES {
            let Some(pc) = registers[PC].map(|pc| pc & !1).filter(|pc| *pc != 0) else {
                break;
            };
            let Some(sp) = registers[SP] else { break };
            let mut lookup = if pc_is_return_address { pc - 1 } else { pc };
            // FreeRTOS plants this function's own address as the return address
            // of task entry points, so it must be matched without the -1.
            let at_pc = self.name_at(pc);
            let stop_here = matches!(at_pc.as_deref(), Some("Reset_Handler" | "prvTaskExitError"));
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
                function,
                lookup_pc: lookup,
                cfa: None,
                registers,
            });
            if stop_here {
                break;
            }

            let unwound = match self.apply_cfi(&mut context, lookup, &registers, memory) {
                Ok(unwound) => unwound,
                Err(SymbolError::Memory(_)) => break,
                Err(error) => return Err(error),
            };
            let (cfa, mut caller) = match unwound {
                Some(result) => {
                    if let Some(frame) = frames.last_mut() {
                        frame.cfa = Some(result.0);
                    }
                    result
                }
                // Assembly leaf: nothing pushed, the return address is in LR.
                None if !pc_is_return_address => (sp, registers),
                None => break,
            };

            let Some(return_address) = caller[LR] else {
                break;
            };
            if is_exc_return(return_address) {
                let stacked_at = if return_address & EXC_RETURN_USES_PSP != 0 {
                    start.psp
                } else {
                    cfa
                };
                let mut saved = [0u32; 8];
                let mut complete = true;
                for (slot, word) in saved.iter_mut().enumerate() {
                    match read_word(memory, stacked_at.wrapping_add(slot as u32 * 4)) {
                        Ok(value) => *word = value,
                        Err(_) => {
                            complete = false;
                            break;
                        }
                    }
                }
                if !complete {
                    break;
                }
                frames.push(StackFrame {
                    kind: FrameKind::Exception {
                        number: xpsr & XPSR_EXCEPTION_MASK,
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
                continue;
            }

            if return_address == 0 || return_address == u32::MAX {
                break;
            }
            if previous_cfa.is_some_and(|previous| cfa <= previous) {
                break;
            }
            caller[PC] = Some(return_address);
            registers = caller;
            previous_cfa = Some(cfa);
            pc_is_return_address = true;
        }
        Ok(frames)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
