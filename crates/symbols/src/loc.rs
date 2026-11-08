//! DWARF location evaluation (DESIGN §5.3.2, §5.4).
//!
//! Anything that cannot be computed reliably reports [`Located::OptimizedOut`]:
//! a missing value is better than a wrong one.

use gimli::{AttributeValue, EvaluationResult, Expression, Location, Value};

use crate::db::SymbolDb;
use crate::index::R;
use crate::scope::LocalVar;
use crate::{MemoryReader, Result, SymbolError};

const FIRST_S_REGISTER: u16 = 64;
const MAX_FRAME_BASE_DEPTH: usize = 2;
const MAX_EVALUATION_STEPS: usize = 256;

/// Register state of one stack frame.
#[derive(Clone, Copy, Debug)]
pub struct FrameRegisters<'a> {
    pub pc: u32,
    pub r: &'a [Option<u32>; 16],
    /// S0-S31. Only known for the innermost frame.
    pub s: Option<&'a [u32; 32]>,
    pub cfa: Option<u32>,
}

impl FrameRegisters<'_> {
    fn dwarf_register(&self, number: u16) -> Option<u32> {
        match number {
            0..=15 => self.r[number as usize],
            n if (FIRST_S_REGISTER..FIRST_S_REGISTER + 32).contains(&n) => {
                self.s.map(|s| s[(n - FIRST_S_REGISTER) as usize])
            }
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Located {
    Memory(u32),
    Bytes(Vec<u8>),
    OptimizedOut,
}

fn value_as_u64(value: Value) -> Option<u64> {
    match value {
        Value::Generic(v) | Value::U64(v) => Some(v),
        Value::I8(v) => Some(v as u8 as u64),
        Value::U8(v) => Some(u64::from(v)),
        Value::I16(v) => Some(v as u16 as u64),
        Value::U16(v) => Some(u64::from(v)),
        Value::I32(v) => Some(v as u32 as u64),
        Value::U32(v) => Some(u64::from(v)),
        Value::I64(v) => Some(v as u64),
        Value::F32(v) => Some(u64::from(v.to_bits())),
        Value::F64(v) => Some(v.to_bits()),
    }
}

impl SymbolDb {
    fn evaluate_expression(
        &self,
        unit: &gimli::Unit<R>,
        expression: Expression<R>,
        frame: &FrameRegisters<'_>,
        frame_base: Option<AttributeValue<R>>,
        memory: &mut dyn MemoryReader,
        depth: usize,
    ) -> Result<Located> {
        let mut evaluation = expression.evaluation(unit.encoding());
        let mut state = evaluation.evaluate()?;
        for _ in 0..MAX_EVALUATION_STEPS {
            state = match state {
                EvaluationResult::Complete => {
                    return self.finish(unit, evaluation.result(), frame);
                }
                EvaluationResult::RequiresMemory {
                    address,
                    size,
                    base_type,
                    ..
                } => {
                    if base_type.0 != 0 || size == 0 || size > 8 {
                        return Ok(Located::OptimizedOut);
                    }
                    let mut bytes = [0u8; 8];
                    memory
                        .read(address as u32, &mut bytes[..size as usize])
                        .map_err(SymbolError::Memory)?;
                    evaluation.resume_with_memory(Value::Generic(u64::from_le_bytes(bytes)))?
                }
                EvaluationResult::RequiresRegister {
                    register,
                    base_type,
                } => {
                    if base_type.0 != 0 {
                        return Ok(Located::OptimizedOut);
                    }
                    let Some(value) = frame.dwarf_register(register.0) else {
                        return Ok(Located::OptimizedOut);
                    };
                    evaluation.resume_with_register(Value::Generic(u64::from(value)))?
                }
                EvaluationResult::RequiresFrameBase => {
                    let base = self.frame_base_value(unit, frame, frame_base, memory, depth)?;
                    let Some(base) = base else {
                        return Ok(Located::OptimizedOut);
                    };
                    evaluation.resume_with_frame_base(u64::from(base))?
                }
                EvaluationResult::RequiresCallFrameCfa => {
                    let Some(cfa) = frame.cfa else {
                        return Ok(Located::OptimizedOut);
                    };
                    evaluation.resume_with_call_frame_cfa(u64::from(cfa))?
                }
                EvaluationResult::RequiresRelocatedAddress(address) => {
                    evaluation.resume_with_relocated_address(address)?
                }
                // The value on entry is not recoverable after the fact.
                _ => return Ok(Located::OptimizedOut),
            };
        }
        Ok(Located::OptimizedOut)
    }

    fn frame_base_value(
        &self,
        unit: &gimli::Unit<R>,
        frame: &FrameRegisters<'_>,
        frame_base: Option<AttributeValue<R>>,
        memory: &mut dyn MemoryReader,
        depth: usize,
    ) -> Result<Option<u32>> {
        let Some(AttributeValue::Exprloc(expression)) = frame_base else {
            return Ok(None);
        };
        if depth >= MAX_FRAME_BASE_DEPTH {
            return Ok(None);
        }
        let located = self.evaluate_expression(unit, expression, frame, None, memory, depth + 1)?;
        Ok(match located {
            // A frame base expression such as DW_OP_reg7 names the base.
            Located::Memory(address) => Some(address),
            Located::Bytes(bytes) if bytes.len() >= 4 => {
                Some(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
            }
            _ => None,
        })
    }

    fn finish(
        &self,
        _unit: &gimli::Unit<R>,
        pieces: Vec<gimli::Piece<R>>,
        frame: &FrameRegisters<'_>,
    ) -> Result<Located> {
        if pieces.is_empty() {
            return Ok(Located::OptimizedOut);
        }
        if pieces.len() == 1 {
            if let Location::Address { address } = pieces[0].location {
                return Ok(Located::Memory(address as u32));
            }
        }
        let mut bytes = Vec::new();
        for piece in pieces {
            let piece_bytes = piece.size_in_bits.map(|bits| (bits / 8) as usize);
            let mut part = match piece.location {
                Location::Register { register } => match frame.dwarf_register(register.0) {
                    Some(value) => value.to_le_bytes().to_vec(),
                    None => return Ok(Located::OptimizedOut),
                },
                Location::Value { value } => match value_as_u64(value) {
                    Some(value) => value.to_le_bytes().to_vec(),
                    None => return Ok(Located::OptimizedOut),
                },
                Location::Bytes { value } => value.slice().to_vec(),
                // Composite objects partly in memory are too rare to justify
                // reading each piece; report them as unavailable.
                _ => return Ok(Located::OptimizedOut),
            };
            if let Some(size) = piece_bytes {
                part.resize(size, 0);
            }
            bytes.extend_from_slice(&part);
        }
        Ok(Located::Bytes(bytes))
    }

    /// Where a local variable lives at this frame's PC.
    pub fn locate(
        &self,
        var: &LocalVar,
        frame: &FrameRegisters<'_>,
        memory: &mut dyn MemoryReader,
    ) -> Result<Located> {
        let unit = self.unit(var.unit)?;
        match var.location {
            Some(AttributeValue::Exprloc(expression)) => {
                return self.evaluate_expression(
                    &unit,
                    expression,
                    frame,
                    var.frame_base,
                    memory,
                    0,
                );
            }
            Some(value) => {
                let Some(mut entries) = self.dwarf.attr_locations(&unit, value)? else {
                    return Ok(Located::OptimizedOut);
                };
                while let Some(entry) = entries.next()? {
                    let pc = u64::from(frame.pc);
                    if pc >= entry.range.begin && pc < entry.range.end {
                        return self.evaluate_expression(
                            &unit,
                            entry.data,
                            frame,
                            var.frame_base,
                            memory,
                            0,
                        );
                    }
                }
                return Ok(Located::OptimizedOut);
            }
            None => {}
        }
        Ok(match var.const_value {
            Some(AttributeValue::Block(block)) => Located::Bytes(block.slice().to_vec()),
            Some(AttributeValue::Sdata(value)) => Located::Bytes(value.to_le_bytes().to_vec()),
            Some(value) => match value.udata_value() {
                Some(value) => Located::Bytes(value.to_le_bytes().to_vec()),
                None => Located::OptimizedOut,
            },
            None => Located::OptimizedOut,
        })
    }
}
