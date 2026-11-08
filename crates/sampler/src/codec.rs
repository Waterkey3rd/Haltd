//! Turning target bytes into numbers, and expressions into sampleable leaves.

use symbols::{BaseEncoding, BitField, MemoryReader, Place, SymbolDb, TypeKind};
use visualizer::Visualizers;

const MAX_LEAVES: usize = 64;
const MAX_DEPTH: usize = 8;

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

/// One numeric value reachable from an expression.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Leaf {
    pub name: String,
    pub place: Place,
    pub scalar: Scalar,
}

/// How a member's name extends its parent's expression.
pub fn joined(parent: &str, child: &str) -> String {
    if child.starts_with('[') {
        format!("{parent}{child}")
    } else if child.starts_with('<') || child == "*" || child == "<anonymous>" {
        parent.to_owned()
    } else {
        format!("{parent}.{child}")
    }
}

fn walk(
    db: &SymbolDb,
    rules: Option<&Visualizers>,
    memory: &mut dyn MemoryReader,
    name: String,
    place: Place,
    depth: usize,
    out: &mut Vec<Leaf>,
) {
    if out.len() >= MAX_LEAVES || depth > MAX_DEPTH {
        return;
    }
    if let Some(scalar) = Scalar::of(db, &place) {
        out.push(Leaf {
            name,
            place,
            scalar,
        });
        return;
    }
    let shaped = rules
        .and_then(|rules| rules.apply(db, memory, place))
        .and_then(|result| result.children)
        .filter(|children| !children.is_empty());
    let children = match shaped {
        Some(children) => children,
        None => db.children(&place, memory).unwrap_or_default(),
    };
    // A wrapper with a single member reads as the value it wraps.
    let single = children.len() == 1;
    for (child, child_place) in children {
        let child_name = if single {
            name.clone()
        } else {
            joined(&name, &child)
        };
        walk(db, rules, memory, child_name, child_place, depth + 1, out);
    }
}

/// Numeric leaves of an object, named from `name`.
pub fn leaves_of_place(
    db: &SymbolDb,
    rules: Option<&Visualizers>,
    name: &str,
    place: Place,
    memory: &mut dyn MemoryReader,
) -> Vec<Leaf> {
    let mut leaves = Vec::new();
    walk(db, rules, memory, name.to_owned(), place, 0, &mut leaves);
    leaves
}

/// Numeric leaves of an expression: itself if scalar, else its members.
pub fn resolve_leaves(
    db: &SymbolDb,
    rules: Option<&Visualizers>,
    expression: &str,
    memory: &mut dyn MemoryReader,
) -> Result<Vec<Leaf>, String> {
    let parsed = symbols::parse(expression).map_err(|e| e.to_string())?;
    let place = match db.evaluate(&parsed, memory).map_err(|e| e.to_string())? {
        symbols::Evaluated::Place(place) => place,
        symbols::Evaluated::Address { .. } => {
            return Err("the result of '&' has no storage to sample".to_owned())
        }
    };
    let mut leaves = Vec::new();
    walk(
        db,
        rules,
        memory,
        expression.to_owned(),
        place,
        0,
        &mut leaves,
    );
    if leaves.is_empty() {
        return Err(format!("{expression} has no numeric value to sample"));
    }
    Ok(leaves)
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
    fn numbers_decode_by_kind_and_width() {
        assert_eq!(scalar(ScalarKind::Unsigned, 1).decode(&[0xFF]), 255.0);
        assert_eq!(scalar(ScalarKind::Signed, 1).decode(&[0xFF]), -1.0);
        assert_eq!(scalar(ScalarKind::Signed, 2).decode(&[0xFE, 0xFF]), -2.0);
        let float = 1.5f32.to_le_bytes();
        assert_eq!(scalar(ScalarKind::Float, 4).decode(&float), 1.5);
        let double = (-0.25f64).to_le_bytes();
        assert_eq!(scalar(ScalarKind::Float, 8).decode(&double), -0.25);
    }

    #[test]
    fn bit_fields_extract_and_sign_extend() {
        let field = Scalar {
            kind: ScalarKind::Signed,
            size: 1,
            bits: Some(BitField {
                bit_offset: 1,
                bit_size: 3,
            }),
        };
        // Bits 1..=3 hold 0b110, which is -2 in three bits.
        assert_eq!(field.decode(&[0b0000_1100]), -2.0);
    }

    #[test]
    fn values_encode_with_range_checks() {
        let byte = scalar(ScalarKind::Unsigned, 1);
        assert_eq!(byte.encode("255"), Ok(255));
        assert_eq!(byte.encode("0x10"), Ok(16));
        assert_eq!(byte.encode("true"), Ok(1));
        assert!(byte.encode("256").is_err());
        assert!(byte.encode("-1").is_err());
        assert!(byte.encode("x").is_err());

        let signed = scalar(ScalarKind::Signed, 1);
        assert_eq!(signed.encode("-1"), Ok(0xFF));
        assert!(signed.encode("-129").is_err());

        let float = scalar(ScalarKind::Float, 4);
        assert_eq!(float.encode("1.5"), Ok(u64::from(1.5f32.to_bits())));
    }

    #[test]
    fn child_names_compose_like_expressions() {
        assert_eq!(joined("a", "b"), "a.b");
        assert_eq!(joined("a", "[2]"), "a[2]");
        assert_eq!(joined("a", "<Base>"), "a");
    }
}
