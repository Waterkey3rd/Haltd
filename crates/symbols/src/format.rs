//! Rendering target values as text.

use crate::db::SymbolDb;
use crate::expr::{parse, Evaluated, Place};
use crate::scalar::Scalar;
use crate::types::{BaseEncoding, BitField, Member, TypeKind, TypeRef};
use crate::{MemoryReader, Result, SymbolError};

/// Largest object rendered in one read.
const MAX_READ: u64 = 1 << 20;
/// Array elements listed in a variable tree.
const MAX_CHILDREN: u64 = 512;

#[derive(Clone, Copy, Debug)]
pub struct FormatOptions {
    /// Array elements shown before eliding the rest.
    pub max_elements: usize,
    /// Nesting levels shown before eliding with `{...}`.
    pub max_depth: usize,
}

impl Default for FormatOptions {
    fn default() -> Self {
        Self {
            max_elements: 32,
            max_depth: 8,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReadResult {
    pub addr: Option<u32>,
    pub type_name: String,
    pub value: String,
}

impl SymbolDb {
    /// Parse `expression`, evaluate it, and render the value.
    pub fn read_expression(
        &self,
        expression: &str,
        memory: &mut dyn MemoryReader,
        options: &FormatOptions,
    ) -> Result<ReadResult> {
        self.read_expression_with(expression, memory, options, &[])
    }

    /// Like [`Self::read_expression`], with locals visible by name.
    pub fn read_expression_with(
        &self,
        expression: &str,
        memory: &mut dyn MemoryReader,
        options: &FormatOptions,
        locals: &[(String, Place)],
    ) -> Result<ReadResult> {
        let parsed = parse(expression)?;
        match self.evaluate_with(&parsed, memory, locals)? {
            Evaluated::Address { value, target } => Ok(ReadResult {
                addr: None,
                type_name: format!("{}*", self.type_name(target)),
                value: format!("{value:#010x}"),
            }),
            Evaluated::Value(number) => Ok(ReadResult {
                addr: None,
                type_name: String::new(),
                value: number.to_string(),
            }),
            Evaluated::Place(place) => {
                let value = self.render_place(&place, memory, options)?;
                Ok(ReadResult {
                    addr: Some(place.addr),
                    type_name: self.type_name(place.ty),
                    value,
                })
            }
        }
    }
}

impl SymbolDb {
    /// Encode a user-entered string for `place`, supporting enum member names
    /// in addition to numeric and character literals.
    pub fn encode_place_value(
        &self,
        place: &Place,
        text: &str,
    ) -> std::result::Result<u64, String> {
        let scalar = Scalar::of(self, place)
            .ok_or_else(|| "only numeric values can be written".to_owned())?;
        let trimmed = text.trim();
        let (_, info) = self.strip(place.ty).map_err(|e| e.to_string())?;
        if let TypeKind::Enum { enumerators } = &info.kind {
            if let Some((_, val)) = enumerators
                .iter()
                .find(|(name, _)| name == trimmed)
                .or_else(|| {
                    enumerators
                        .iter()
                        .find(|(name, _)| name.rsplit("::").next() == Some(trimmed))
                })
            {
                return scalar.encode_int(i128::from(*val), trimmed);
            }
        }
        scalar.encode(trimmed)
    }

    /// Bytes occupied by `place`.
    pub fn place_size(&self, place: &Place) -> Result<u64> {
        Ok(match place.bits {
            Some(bits) => u64::from((bits.bit_offset + bits.bit_size).div_ceil(8)),
            None => self.size_of(place.ty, place.consumed)?,
        })
    }

    pub fn render_place(
        &self,
        place: &Place,
        memory: &mut dyn MemoryReader,
        options: &FormatOptions,
    ) -> Result<String> {
        let length = self.place_size(place)?;
        if length > MAX_READ {
            return Err(SymbolError::Type(format!(
                "object of {length} bytes is too large to display"
            )));
        }
        let mut bytes = vec![0u8; length as usize];
        memory
            .read(place.addr, &mut bytes)
            .map_err(SymbolError::Memory)?;
        let renderer = Renderer { db: self, options };
        let mut value = String::new();
        match place.bits {
            Some(bits) => renderer.bitfield(place.ty, &bytes, bits, &mut value)?,
            None => renderer.render(place.ty, place.consumed, &bytes, 0, &mut value)?,
        }
        Ok(value)
    }

    /// Direct sub-objects for a variable tree; empty for scalars. Pointers are
    /// followed by reading the target, so a pointer has one child: its pointee.
    pub fn children(
        &self,
        place: &Place,
        memory: &mut dyn MemoryReader,
    ) -> Result<Vec<(String, Place)>> {
        if place.bits.is_some() {
            return Ok(Vec::new());
        }
        let (_, info) = self.strip(place.ty)?;
        let mut children = Vec::new();
        match &info.kind {
            TypeKind::Struct { members, .. } => {
                for member in members {
                    let Some(ty) = member.ty else { continue };
                    let name = match (&member.name, member.is_base) {
                        (_, true) => format!("<{}>", self.type_name(ty)),
                        (Some(name), false) => name.clone(),
                        (None, false) => "<anonymous>".to_owned(),
                    };
                    children.push((
                        name,
                        Place {
                            addr: place.addr + member.offset as u32,
                            ty,
                            consumed: 0,
                            bits: member.bits,
                        },
                    ));
                }
            }
            TypeKind::Array {
                element: Some(element),
                dimensions,
            } => {
                let remaining = &dimensions[place.consumed.min(dimensions.len())..];
                let count = remaining.first().copied().flatten().unwrap_or(0);
                let mut stride = self.size_of(*element, 0)? as u32;
                for dimension in remaining.iter().skip(1) {
                    stride *= dimension.unwrap_or(0) as u32;
                }
                for index in 0..count.min(MAX_CHILDREN) {
                    let addr = place.addr + index as u32 * stride;
                    let child = if remaining.len() <= 1 {
                        Place::new(addr, *element)
                    } else {
                        Place {
                            addr,
                            ty: place.ty,
                            consumed: place.consumed + 1,
                            bits: None,
                        }
                    };
                    children.push((format!("[{index}]"), child));
                }
            }
            TypeKind::Pointer(Some(target)) | TypeKind::Reference(Some(target)) => {
                let (_, pointee) = self.strip(*target)?;
                if !matches!(pointee.kind, TypeKind::Function | TypeKind::Unknown) {
                    let mut bytes = [0u8; 4];
                    memory
                        .read(place.addr, &mut bytes)
                        .map_err(SymbolError::Memory)?;
                    let value = u32::from_le_bytes(bytes);
                    if value != 0 {
                        children.push(("*".to_owned(), Place::new(value, *target)));
                    }
                }
            }
            _ => {}
        }
        Ok(children)
    }
}

struct Renderer<'a> {
    db: &'a SymbolDb,
    options: &'a FormatOptions,
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

fn escape_char(byte: u8, out: &mut String) {
    match byte {
        b'"' => out.push_str("\\\""),
        b'\\' => out.push_str("\\\\"),
        b'\n' => out.push_str("\\n"),
        b'\r' => out.push_str("\\r"),
        b'\t' => out.push_str("\\t"),
        0x20..=0x7E => out.push(byte as char),
        other => out.push_str(&format!("\\x{other:02x}")),
    }
}

impl Renderer<'_> {
    fn render(
        &self,
        ty: TypeRef,
        consumed: usize,
        bytes: &[u8],
        depth: usize,
        out: &mut String,
    ) -> Result<()> {
        let (_, info) = self.db.strip(ty)?;
        match &info.kind {
            TypeKind::Base(encoding) => {
                self.scalar(*encoding, little_endian(bytes), bytes.len() as u32 * 8, out);
            }
            TypeKind::Pointer(_) | TypeKind::Reference(_) => {
                out.push_str(&format!("{:#010x}", little_endian(bytes)));
            }
            TypeKind::Enum { enumerators } => {
                let value = sign_extend(little_endian(bytes), bytes.len() as u32 * 8);
                self.enumerated(enumerators, value, out);
            }
            TypeKind::Struct { members, .. } => {
                if depth >= self.options.max_depth {
                    out.push_str("{...}");
                } else {
                    self.structure(members, bytes, depth, out)?;
                }
            }
            TypeKind::Array {
                element,
                dimensions,
            } => {
                let element = element
                    .ok_or_else(|| SymbolError::Type("array of unknown element".to_owned()))?;
                self.array(
                    ty,
                    element,
                    &dimensions[consumed..],
                    consumed,
                    bytes,
                    depth,
                    out,
                )?;
            }
            _ => out.push_str("<unsupported>"),
        }
        Ok(())
    }

    fn scalar(&self, encoding: BaseEncoding, raw: u64, bits: u32, out: &mut String) {
        match encoding {
            BaseEncoding::Bool => out.push_str(if raw != 0 { "true" } else { "false" }),
            BaseEncoding::Float if bits == 32 => {
                out.push_str(&f32::from_bits(raw as u32).to_string());
            }
            BaseEncoding::Float if bits == 64 => out.push_str(&f64::from_bits(raw).to_string()),
            BaseEncoding::Signed => out.push_str(&sign_extend(raw, bits).to_string()),
            BaseEncoding::SignedChar | BaseEncoding::UnsignedChar if bits == 8 => {
                let number = if encoding == BaseEncoding::SignedChar {
                    sign_extend(raw, 8)
                } else {
                    raw as i64
                };
                out.push_str(&format!("{number} '"));
                escape_char(raw as u8, out);
                out.push('\'');
            }
            BaseEncoding::SignedChar => out.push_str(&sign_extend(raw, bits).to_string()),
            _ => out.push_str(&raw.to_string()),
        }
    }

    fn enumerated(&self, enumerators: &[(String, i64)], value: i64, out: &mut String) {
        match enumerators.iter().find(|(_, v)| *v == value) {
            Some((name, _)) => out.push_str(name),
            None => out.push_str(&format!("<unknown enumerator> ({value})")),
        }
    }

    fn structure(
        &self,
        members: &[Member],
        bytes: &[u8],
        depth: usize,
        out: &mut String,
    ) -> Result<()> {
        out.push('{');
        for (position, member) in members.iter().enumerate() {
            if position > 0 {
                out.push_str(", ");
            }
            let Some(ty) = member.ty else { continue };
            if member.is_base {
                out.push_str(&format!("<{}> = ", self.db.type_name(ty)));
            } else if let Some(name) = &member.name {
                out.push_str(&format!("{name} = "));
            }
            let start = member.offset as usize;
            if let Some(bits) = member.bits {
                let end = start + (bits.bit_offset + bits.bit_size).div_ceil(8) as usize;
                let slice = bytes.get(start..end).ok_or_else(|| truncated(member))?;
                self.bitfield(ty, slice, bits, out)?;
            } else {
                let size = self.db.size_of(ty, 0)? as usize;
                let slice = bytes
                    .get(start..start + size)
                    .ok_or_else(|| truncated(member))?;
                self.render(ty, 0, slice, depth + 1, out)?;
            }
        }
        out.push('}');
        Ok(())
    }

    fn bitfield(&self, ty: TypeRef, bytes: &[u8], bits: BitField, out: &mut String) -> Result<()> {
        let mut raw = little_endian(bytes) >> bits.bit_offset;
        if bits.bit_size < 64 {
            raw &= (1u64 << bits.bit_size) - 1;
        }
        let (_, info) = self.db.strip(ty)?;
        match &info.kind {
            TypeKind::Base(BaseEncoding::Bool) => {
                self.scalar(BaseEncoding::Bool, raw, bits.bit_size, out);
            }
            TypeKind::Base(BaseEncoding::Signed | BaseEncoding::SignedChar) => {
                out.push_str(&sign_extend(raw, bits.bit_size).to_string());
            }
            TypeKind::Enum { enumerators } => {
                self.enumerated(enumerators, raw as i64, out);
            }
            _ => out.push_str(&raw.to_string()),
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn array(
        &self,
        ty: TypeRef,
        element: TypeRef,
        dimensions: &[Option<u64>],
        consumed: usize,
        bytes: &[u8],
        depth: usize,
        out: &mut String,
    ) -> Result<()> {
        let count = dimensions.first().copied().flatten().unwrap_or(0) as usize;
        let inner_dimensions = dimensions.len() > 1;
        if !inner_dimensions && self.is_char(element)? {
            self.string(&bytes[..count.min(bytes.len())], out);
            return Ok(());
        }
        if depth >= self.options.max_depth {
            out.push_str("[...]");
            return Ok(());
        }
        let stride = bytes.len().checked_div(count).unwrap_or(0);
        out.push('[');
        for position in 0..count.min(self.options.max_elements) {
            if position > 0 {
                out.push_str(", ");
            }
            let slice = &bytes[position * stride..(position + 1) * stride];
            if inner_dimensions {
                self.render(ty, consumed + 1, slice, depth + 1, out)?;
            } else {
                self.render(element, 0, slice, depth + 1, out)?;
            }
        }
        if count > self.options.max_elements {
            out.push_str(&format!(", ... ({count} total)"));
        }
        out.push(']');
        Ok(())
    }

    fn is_char(&self, ty: TypeRef) -> Result<bool> {
        let (_, info) = self.db.strip(ty)?;
        Ok(matches!(
            info.kind,
            TypeKind::Base(BaseEncoding::SignedChar | BaseEncoding::UnsignedChar)
        ) && info.size == Some(1))
    }

    fn string(&self, bytes: &[u8], out: &mut String) {
        let end = bytes.iter().position(|b| *b == 0).unwrap_or(bytes.len());
        out.push('"');
        for byte in &bytes[..end] {
            escape_char(*byte, out);
        }
        out.push('"');
    }
}

fn truncated(member: &Member) -> SymbolError {
    SymbolError::Type(format!(
        "member {} lies outside its parent object",
        member.name.as_deref().unwrap_or("<anonymous>")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sign_extension_uses_the_field_width() {
        assert_eq!(sign_extend(0b101, 3), -3);
        assert_eq!(sign_extend(0b011, 3), 3);
        assert_eq!(sign_extend(0xFFFF_FFFF, 32), -1);
        assert_eq!(sign_extend(u64::MAX, 64), -1);
    }

    #[test]
    fn little_endian_reads_up_to_eight_bytes() {
        assert_eq!(little_endian(&[0x34, 0x12]), 0x1234);
        assert_eq!(little_endian(&[]), 0);
    }

    #[test]
    fn strings_are_escaped() {
        let mut out = String::new();
        for byte in [b'a', b'"', b'\n', 0x01] {
            escape_char(byte, &mut out);
        }
        assert_eq!(out, "a\\\"\\n\\x01");
    }
}
