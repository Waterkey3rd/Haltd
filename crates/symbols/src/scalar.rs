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

    /// Bytes to store for `text`: decimal/hex/binary numbers (with optional `_`
    /// separators), `true`/`false`, or single-byte character literals `'A'`.
    pub fn encode(&self, text: &str) -> Result<u64, String> {
        let text = text.trim();
        let bad = || format!("{text:?} is not a valid value");
        if self.kind == ScalarKind::Float {
            let cleaned = strip_numeric_separators(text).ok_or_else(bad)?;
            let value: f64 = cleaned.parse().map_err(|_| bad())?;
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
        let width = self.bits.map_or(self.size * 8, |bits| bits.bit_size);
        if width == 0 || width > 64 {
            return Err(format!("unsupported scalar width {width}"));
        }
        let parsed = if let Some(ch) = parse_char_literal(text) {
            if width < 8 && u32::from(ch) >= (1u32 << width) {
                return Err(format!("{text} does not fit in {width} bits"));
            }
            i128::from(ch)
        } else {
            let cleaned = strip_numeric_separators(text).ok_or_else(bad)?;
            match cleaned
                .strip_prefix("0x")
                .or_else(|| cleaned.strip_prefix("0X"))
            {
                Some(hex) => i128::from_str_radix(hex, 16),
                None => match cleaned
                    .strip_prefix("0b")
                    .or_else(|| cleaned.strip_prefix("0B"))
                {
                    Some(bin) => i128::from_str_radix(bin, 2),
                    None => cleaned.parse::<i128>(),
                },
            }
            .map_err(|_| bad())?
        };
        self.encode_int(parsed, text)
    }

    /// Range-check an integer value and mask it to the scalar's bit width.
    pub fn encode_int(&self, parsed: i128, label: &str) -> Result<u64, String> {
        let width = self.bits.map_or(self.size * 8, |bits| bits.bit_size);
        if width == 0 || width > 64 {
            return Err(format!("unsupported scalar width {width}"));
        }
        let (low, high) = match self.kind {
            ScalarKind::Signed => (-(1i128 << (width - 1)), (1i128 << width) - 1),
            _ => (0, (1i128 << width) - 1),
        };
        if parsed < low || parsed > high {
            return Err(format!("{label} does not fit in {width} bits"));
        }
        Ok(parsed as u64 & (u64::MAX >> (64 - width)))
    }
}

fn strip_numeric_separators(text: &str) -> Option<String> {
    if text.is_empty() || text.starts_with('_') || text.ends_with('_') || text.contains("__") {
        return None;
    }
    Some(text.replace('_', ""))
}

fn parse_char_literal(text: &str) -> Option<u8> {
    let inner = text.strip_prefix('\'')?.strip_suffix('\'')?;
    if let Some(esc) = inner.strip_prefix('\\') {
        return match esc {
            "n" => Some(b'\n'),
            "r" => Some(b'\r'),
            "t" => Some(b'\t'),
            "0" => Some(b'\0'),
            "\\" => Some(b'\\'),
            "'" => Some(b'\''),
            "\"" => Some(b'"'),
            _ => esc
                .strip_prefix('x')
                .filter(|hex| hex.len() == 2)
                .and_then(|hex| u8::from_str_radix(hex, 16).ok()),
        };
    }
    let mut chars = inner.chars();
    let ch = chars.next()?;
    (chars.next().is_none() && u32::from(ch) <= 0xFF).then_some(ch as u8)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scalar(kind: ScalarKind, size: u32) -> Scalar {
        Scalar {
            kind,
            size,
            bits: None,
        }
    }

    #[test]
    fn encodes_binary_underscores_and_char_literals() {
        let u8s = scalar(ScalarKind::Unsigned, 1);
        assert_eq!(u8s.encode("0b1010_0101"), Ok(0b1010_0101));
        assert_eq!(u8s.encode("0B1111"), Ok(0b1111));
        assert_eq!(u8s.encode("'A'"), Ok(65));
        assert_eq!(u8s.encode("'\\n'"), Ok(10));
        assert_eq!(u8s.encode("'\\x7f'"), Ok(0x7F));
        assert!(u8s.encode("_12").is_err());

        let u32s = scalar(ScalarKind::Unsigned, 4);
        assert_eq!(u32s.encode("1_000_000"), Ok(1_000_000));
        assert_eq!(u32s.encode("0xFF00_1234"), Ok(0xFF00_1234));

        let f32s = scalar(ScalarKind::Float, 4);
        assert_eq!(f32s.encode("1_234.5"), Ok(u64::from(1234.5f32.to_bits())));
    }
}
