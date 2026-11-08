//! Lazily parsed type information.
//!
//! A [`Type`] describes one DIE and refers to others by [`TypeRef`], so
//! self-referencing types need no special handling.

use std::sync::Arc;

use gimli::{constants as dw, AttributeValue, UnitOffset};

use crate::db::SymbolDb;
use crate::index::{self, R};
use crate::{Result, SymbolError};

const MAX_TYPE_CHAIN: usize = 32;

/// Location of a type DIE: section offset of its unit, offset within the unit.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct TypeRef {
    pub unit: u32,
    pub die: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BaseEncoding {
    Bool,
    Float,
    Signed,
    SignedChar,
    Unsigned,
    UnsignedChar,
    Other,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BitField {
    /// Bit position relative to the byte the member's offset points at.
    pub bit_offset: u32,
    pub bit_size: u32,
}

#[derive(Clone, Debug)]
pub struct Member {
    /// None for anonymous members and base classes.
    pub name: Option<String>,
    pub ty: Option<TypeRef>,
    pub offset: u64,
    pub bits: Option<BitField>,
    pub is_base: bool,
}

#[derive(Clone, Debug)]
pub enum TypeKind {
    Base(BaseEncoding),
    Pointer(Option<TypeRef>),
    Reference(Option<TypeRef>),
    Typedef(Option<TypeRef>),
    /// const, volatile, restrict, atomic.
    Qualified(Option<TypeRef>),
    Struct {
        members: Vec<Member>,
        declaration: bool,
    },
    Array {
        element: Option<TypeRef>,
        /// Element count per dimension; None for an unknown bound.
        dimensions: Vec<Option<u64>>,
    },
    Enum {
        enumerators: Vec<(String, i64)>,
    },
    Function,
    Unknown,
}

#[derive(Clone, Debug)]
pub struct Type {
    pub name: Option<String>,
    pub size: Option<u64>,
    pub kind: TypeKind,
}

fn signed_value(value: AttributeValue<R>) -> Option<i64> {
    match value {
        AttributeValue::Sdata(v) => Some(v),
        other => other.udata_value().map(|v| v as i64),
    }
}

/// Handle both the constant form and the DWARF 2 `DW_OP_plus_uconst` form.
fn member_offset(value: AttributeValue<R>) -> Option<u64> {
    const DW_OP_PLUS_UCONST: u8 = 0x23;
    if let AttributeValue::Exprloc(expression) = value {
        let bytes = expression.0.slice();
        if bytes.first() != Some(&DW_OP_PLUS_UCONST) {
            return None;
        }
        let mut result = 0u64;
        for (position, byte) in bytes[1..].iter().enumerate() {
            result |= u64::from(byte & 0x7F) << (position * 7);
            if byte & 0x80 == 0 {
                return Some(result);
            }
        }
        return None;
    }
    value.udata_value()
}

impl SymbolDb {
    /// Parsed information for one DIE. Results are cached.
    pub fn type_info(&self, reference: TypeRef) -> Result<Arc<Type>> {
        if let Some(found) = self
            .type_cache
            .lock()
            .expect("type cache lock")
            .get(&reference)
        {
            return Ok(found.clone());
        }
        let parsed = Arc::new(self.parse_type(reference)?);
        self.type_cache
            .lock()
            .expect("type cache lock")
            .insert(reference, parsed.clone());
        Ok(parsed)
    }

    fn parse_type(&self, reference: TypeRef) -> Result<Type> {
        let unit = self.unit(reference.unit)?;
        let unit_ref = unit.unit_ref(&self.dwarf);
        let mut tree = unit_ref.entries_tree(Some(UnitOffset(reference.die as usize)))?;
        let root = tree.root()?;
        let entry = root.entry();

        let tag = entry.tag();
        let name = match entry.attr_value(dw::DW_AT_name) {
            Some(value) => {
                Some(String::from_utf8_lossy(unit_ref.attr_string(value)?.slice()).into_owned())
            }
            None => None,
        };
        let size = entry
            .attr_value(dw::DW_AT_byte_size)
            .and_then(|value| value.udata_value());
        let target = entry
            .attr_value(dw::DW_AT_type)
            .and_then(|value| self.resolve_ref(reference.unit, value));
        let encoding = entry.attr_value(dw::DW_AT_encoding);
        let declaration = entry.attr_value(dw::DW_AT_declaration).is_some();

        let kind = match tag {
            dw::DW_TAG_base_type => TypeKind::Base(match encoding {
                Some(AttributeValue::Encoding(dw::DW_ATE_boolean)) => BaseEncoding::Bool,
                Some(AttributeValue::Encoding(dw::DW_ATE_float)) => BaseEncoding::Float,
                Some(AttributeValue::Encoding(dw::DW_ATE_signed)) => BaseEncoding::Signed,
                Some(AttributeValue::Encoding(dw::DW_ATE_signed_char)) => BaseEncoding::SignedChar,
                Some(AttributeValue::Encoding(dw::DW_ATE_unsigned))
                | Some(AttributeValue::Encoding(dw::DW_ATE_UTF)) => BaseEncoding::Unsigned,
                Some(AttributeValue::Encoding(dw::DW_ATE_unsigned_char)) => {
                    BaseEncoding::UnsignedChar
                }
                _ => BaseEncoding::Other,
            }),
            dw::DW_TAG_pointer_type | dw::DW_TAG_ptr_to_member_type => TypeKind::Pointer(target),
            dw::DW_TAG_reference_type | dw::DW_TAG_rvalue_reference_type => {
                TypeKind::Reference(target)
            }
            dw::DW_TAG_typedef => TypeKind::Typedef(target),
            dw::DW_TAG_const_type
            | dw::DW_TAG_volatile_type
            | dw::DW_TAG_restrict_type
            | dw::DW_TAG_atomic_type => TypeKind::Qualified(target),
            dw::DW_TAG_structure_type | dw::DW_TAG_class_type | dw::DW_TAG_union_type => {
                let mut members = Vec::new();
                let mut children = root.children();
                while let Some(child) = children.next()? {
                    let entry = child.entry();
                    let is_base = entry.tag() == dw::DW_TAG_inheritance;
                    // Static members are separate variables, not instance data.
                    let is_static = entry.attr_value(dw::DW_AT_external).is_some()
                        || entry.attr_value(dw::DW_AT_declaration).is_some();
                    if !(is_base || entry.tag() == dw::DW_TAG_member) || is_static {
                        continue;
                    }
                    members.push(self.parse_member(reference.unit, entry, is_base)?);
                }
                TypeKind::Struct {
                    members,
                    declaration,
                }
            }
            dw::DW_TAG_array_type => {
                let mut dimensions = Vec::new();
                let mut children = root.children();
                while let Some(child) = children.next()? {
                    let entry = child.entry();
                    if entry.tag() == dw::DW_TAG_subrange_type {
                        let count = entry
                            .attr_value(dw::DW_AT_count)
                            .and_then(|value| value.udata_value());
                        let upper = entry
                            .attr_value(dw::DW_AT_upper_bound)
                            .and_then(signed_value);
                        dimensions.push(count.or_else(|| upper.map(|u| (u + 1).max(0) as u64)));
                    }
                }
                TypeKind::Array {
                    element: target,
                    dimensions,
                }
            }
            dw::DW_TAG_enumeration_type => {
                let mut enumerators = Vec::new();
                let mut children = root.children();
                while let Some(child) = children.next()? {
                    let entry = child.entry();
                    if entry.tag() != dw::DW_TAG_enumerator {
                        continue;
                    }
                    let name = match entry.attr_value(dw::DW_AT_name) {
                        Some(value) => {
                            String::from_utf8_lossy(unit_ref.attr_string(value)?.slice())
                                .into_owned()
                        }
                        None => continue,
                    };
                    let value = entry
                        .attr_value(dw::DW_AT_const_value)
                        .and_then(signed_value)
                        .unwrap_or(0);
                    enumerators.push((name, value));
                }
                TypeKind::Enum { enumerators }
            }
            dw::DW_TAG_subroutine_type | dw::DW_TAG_subprogram => TypeKind::Function,
            _ => TypeKind::Unknown,
        };
        Ok(Type { name, size, kind })
    }

    fn parse_member(
        &self,
        unit: u32,
        entry: &gimli::DebuggingInformationEntry<R>,
        is_base: bool,
    ) -> Result<Member> {
        let unit_data = self.unit(unit)?;
        let unit_ref = unit_data.unit_ref(&self.dwarf);
        let name = match entry.attr_value(dw::DW_AT_name) {
            Some(value) => {
                Some(String::from_utf8_lossy(unit_ref.attr_string(value)?.slice()).into_owned())
            }
            None => None,
        };
        let ty = entry
            .attr_value(dw::DW_AT_type)
            .and_then(|value| self.resolve_ref(unit, value));
        let mut offset = entry
            .attr_value(dw::DW_AT_data_member_location)
            .and_then(member_offset)
            .unwrap_or(0);

        let bit_size = entry
            .attr_value(dw::DW_AT_bit_size)
            .and_then(|value| value.udata_value());
        let bits = match bit_size {
            None => None,
            Some(bit_size) => {
                let data_bit_offset = entry
                    .attr_value(dw::DW_AT_data_bit_offset)
                    .and_then(|value| value.udata_value());
                let legacy_bit_offset = entry
                    .attr_value(dw::DW_AT_bit_offset)
                    .and_then(|value| value.udata_value());
                let storage_bits = entry
                    .attr_value(dw::DW_AT_byte_size)
                    .and_then(|value| value.udata_value())
                    .map(|bytes| bytes * 8);
                let start = match (data_bit_offset, legacy_bit_offset, storage_bits) {
                    (Some(start), _, _) => start,
                    // DWARF 2/3 counts from the storage unit's high bit.
                    (None, Some(legacy), Some(storage)) => {
                        (offset * 8 + storage).saturating_sub(legacy + bit_size)
                    }
                    _ => offset * 8,
                };
                offset = start / 8;
                Some(BitField {
                    bit_offset: (start % 8) as u32,
                    bit_size: bit_size as u32,
                })
            }
        };
        Ok(Member {
            name,
            ty,
            offset,
            bits,
            is_base,
        })
    }

    /// Replace a forward declaration by its definition from any unit.
    fn complete(&self, reference: TypeRef, info: Arc<Type>) -> Result<(TypeRef, Arc<Type>)> {
        let is_declaration = matches!(
            info.kind,
            TypeKind::Struct {
                declaration: true,
                ..
            }
        );
        if !is_declaration {
            return Ok((reference, info));
        }
        let unit = self.unit(reference.unit)?;
        let Some(name) = index::qualified_name(&self.dwarf, &unit, reference.die)? else {
            return Ok((reference, info));
        };
        match self.type_by_name(&name) {
            Some(definition) => Ok((definition, self.type_info(definition)?)),
            None => Ok((reference, info)),
        }
    }

    /// Follow typedefs and qualifiers, completing forward declarations.
    pub fn strip(&self, reference: TypeRef) -> Result<(TypeRef, Arc<Type>)> {
        let mut current = reference;
        for _ in 0..MAX_TYPE_CHAIN {
            let info = self.type_info(current)?;
            match info.kind {
                TypeKind::Typedef(Some(next)) | TypeKind::Qualified(Some(next)) => current = next,
                _ => return self.complete(current, info),
            }
        }
        Err(SymbolError::Type("type chain is too long".to_owned()))
    }

    /// Size in bytes, skipping the first `consumed` array dimensions.
    pub fn size_of(&self, reference: TypeRef, consumed: usize) -> Result<u64> {
        let (_, info) = self.strip(reference)?;
        if let TypeKind::Array {
            element: Some(element),
            dimensions,
        } = &info.kind
        {
            let mut size = self.size_of(*element, 0)?;
            for dimension in dimensions.iter().skip(consumed) {
                size = size.saturating_mul(dimension.unwrap_or(0));
            }
            return Ok(size);
        }
        match (&info.kind, info.size) {
            (_, Some(size)) => Ok(size),
            (TypeKind::Pointer(_) | TypeKind::Reference(_), None) => Ok(4),
            _ => Err(SymbolError::Type(format!(
                "the size of {} is unknown",
                self.type_name(reference)
            ))),
        }
    }

    /// Name with its enclosing namespaces and classes, as visualizer rules
    /// match it. Rescans the unit once per type, then answers from a cache.
    pub fn qualified_type_name(&self, reference: TypeRef) -> Result<Option<String>> {
        if let Some(found) = self
            .qualified_names
            .lock()
            .expect("qualified name lock")
            .get(&reference)
        {
            return Ok(found.clone());
        }
        let unit = self.unit(reference.unit)?;
        let name = index::qualified_name(&self.dwarf, &unit, reference.die)?;
        self.qualified_names
            .lock()
            .expect("qualified name lock")
            .insert(reference, name.clone());
        Ok(name)
    }

    /// C-like spelling of a type, for display.
    pub fn type_name(&self, reference: TypeRef) -> String {
        self.type_name_inner(reference, 0)
    }

    fn type_name_inner(&self, reference: TypeRef, depth: usize) -> String {
        if depth > MAX_TYPE_CHAIN {
            return "...".to_owned();
        }
        let Ok(info) = self.type_info(reference) else {
            return "<error>".to_owned();
        };
        let named = |target: Option<TypeRef>| match target {
            Some(target) => self.type_name_inner(target, depth + 1),
            None => "void".to_owned(),
        };
        match &info.kind {
            TypeKind::Pointer(target) => format!("{}*", named(*target)),
            TypeKind::Reference(target) => format!("{}&", named(*target)),
            TypeKind::Qualified(target) => named(*target),
            TypeKind::Array {
                element,
                dimensions,
            } => {
                let mut text = named(*element);
                for dimension in dimensions {
                    match dimension {
                        Some(count) => text.push_str(&format!("[{count}]")),
                        None => text.push_str("[]"),
                    }
                }
                text
            }
            TypeKind::Function => "<function>".to_owned(),
            _ => info
                .name
                .clone()
                .unwrap_or_else(|| "<anonymous>".to_owned()),
        }
    }

    /// Find a member by name, descending into anonymous members and bases.
    /// The returned offset is relative to the start of `reference`.
    pub fn find_member(&self, reference: TypeRef, name: &str) -> Result<Option<(u64, Member)>> {
        self.find_member_at(reference, name, 0)
    }

    fn find_member_at(
        &self,
        reference: TypeRef,
        name: &str,
        depth: usize,
    ) -> Result<Option<(u64, Member)>> {
        if depth > MAX_TYPE_CHAIN {
            return Ok(None);
        }
        let (_, info) = self.strip(reference)?;
        let TypeKind::Struct { members, .. } = &info.kind else {
            return Ok(None);
        };
        if let Some(found) = members.iter().find(|m| m.name.as_deref() == Some(name)) {
            return Ok(Some((found.offset, found.clone())));
        }
        for member in members.iter().filter(|m| m.name.is_none() || m.is_base) {
            let Some(ty) = member.ty else { continue };
            if let Some((inner, found)) = self.find_member_at(ty, name, depth + 1)? {
                return Ok(Some((member.offset + inner, found)));
            }
        }
        Ok(None)
    }
}
