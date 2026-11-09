//! Decoding target bytes as numbers, and numbers as target bytes.

use crate::expr::Place;
use crate::types::{BaseEncoding, BitField, TypeKind};
use crate::SymbolDb;

/// A computed value in an expression.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Number {
    Int(i128),
    Float(f64),
}

impl Number {
    pub fn is_true(self) -> bool {
        match self {
            Number::Int(value) => value != 0,
            Number::Float(value) => value != 0.0,
        }
    }

    pub fn as_f64(self) -> f64 {
        match self {
            Number::Int(value) => value as f64,
            Number::Float(value) => value,
        }
    }
}

impl std::fmt::Display for Number {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Number::Int(value) => write!(out, "{value}"),
            Number::Float(value) => write!(out, "{value}"),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScalarKind {
    Unsigned,
    Signed,
    Float,
}

/// How to decode one scalar.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Scalar {
    pub kind: ScalarKind,
    pub size: u32,
    pub bits: Option<BitField>,
}

fn little_endian(bytes: &[u8]) -> u64 {
    bytes
        .iter()
        .take(8)
        .rev()
        .fold(0, |value, byte| (value << 8) | u64::from(*byte))
}

fn sign_extend(value: u64, bits: u32) -> i64 {
    if bits == 0 || bits >= 64 {
        return value as i64;
    }
    let shift = 64 - bits;
    ((value << shift) as i64) >> shift
}

impl Scalar {
    /// Decoding for `place`, or None if it is not a scalar.
    pub fn of(db: &SymbolDb, place: &Place) -> Option<Self> {
        let (_, info) = db.strip(place.ty).ok()?;
        let kind = match &info.kind {
            TypeKind::Base(BaseEncoding::Float) => ScalarKind::Float,
            TypeKind::Base(BaseEncoding::Signed | BaseEncoding::SignedChar) => ScalarKind::Signed,
            TypeKind::Base(_) | TypeKind::Pointer(_) => ScalarKind::Unsigned,
            TypeKind::Enum { .. } => ScalarKind::Signed,
            _ => return None,
        };
        let size = db.place_size(place).ok()? as u32;
        (1..=8).contains(&size).then_some(Self {
            kind,
            size,
            bits: place.bits,
        })
    }

    pub fn decode(&self, bytes: &[u8]) -> f64 {
        let raw = little_endian(&bytes[..self.size as usize]);
        if let Some(bits) = self.bits {
            let mut value = raw >> bits.bit_offset;
            if bits.bit_size < 64 {
                value &= (1u64 << bits.bit_size) - 1;
            }
            return match self.kind {
                ScalarKind::Signed => sign_extend(value, bits.bit_size) as f64,
                _ => value as f64,
            };
        }
        match (self.kind, self.size) {
            (ScalarKind::Float, 4) => f64::from(f32::from_bits(raw as u32)),
            (ScalarKind::Float, 8) => f64::from_bits(raw),
            (ScalarKind::Signed, size) => sign_extend(raw, size * 8) as f64,
            _ => raw as f64,
        }
    }

    /// Like [`Self::decode`], keeping every bit of a 64-bit integer.
    pub fn number(&self, bytes: &[u8]) -> Number {
        if self.kind == ScalarKind::Float && self.bits.is_none() {
            return Number::Float(self.decode(bytes));
        }
        let raw = little_endian(&bytes[..self.size as usize]);
        let (value, width) = match self.bits {
            Some(bits) if bits.bit_size < 64 => (
                (raw >> bits.bit_offset) & ((1u64 << bits.bit_size) - 1),
                bits.bit_size,
            ),
            _ => (raw, self.size * 8),
        };
        Number::Int(match self.kind {
            ScalarKind::Signed => i128::from(sign_extend(value, width)),
            _ => i128::from(value),
        })
    }

    /// Bytes to store for `text`, a number or `true`/`false`.
    pub fn encode(&self, text: &str) -> Result<u64, String> {
        let text = text.trim();
        let bad = || format!("{text:?} is not a valid value");
        if self.kind == ScalarKind::Float {
            let value: f64 = text.parse().map_err(|_| bad())?;
            return Ok(if self.size == 4 {
                u64::from((value as f32).to_bits())
            } else {
                value.to_bits()
            });
        }
        match text {
            "true" => return Ok(1),
            "false" => return Ok(0),
            _ => {}
        }
        let parsed = match text.strip_prefix("0x").or_else(|| text.strip_prefix("0X")) {
            Some(hex) => i128::from_str_radix(hex, 16),
            None => text.parse::<i128>(),
        }
        .map_err(|_| bad())?;
        let width = self.bits.map_or(self.size * 8, |bits| bits.bit_size);
        let (low, high) = match self.kind {
            ScalarKind::Signed => (-(1i128 << (width - 1)), (1i128 << width) - 1),
            _ => (0, (1i128 << width) - 1),
        };
        if parsed < low || parsed > high {
            return Err(format!("{text} does not fit in {width} bits"));
        }
        Ok(parsed as u64 & (u64::MAX >> (64 - width)))
    }
}
