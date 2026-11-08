//! Parser and evaluator for variable path expressions such as
//! `ns::g_state.items[2].gain` or `*g_ptr->next`.
//!
//! This is the M0 subset of DESIGN §5.3.2; arithmetic, casts and comparisons
//! arrive with conditional breakpoints.

use crate::db::{SymbolDb, Variable};
use crate::types::{BitField, TypeKind, TypeRef};
use crate::{MemoryReader, Result, SymbolError};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Expr {
    Path(String),
    Member(Box<Expr>, String),
    Arrow(Box<Expr>, String),
    Index(Box<Expr>, i64),
    Deref(Box<Expr>),
    AddressOf(Box<Expr>),
}

pub fn parse(text: &str) -> Result<Expr> {
    let mut parser = Parser {
        text: text.as_bytes(),
        position: 0,
    };
    let expression = parser.unary()?;
    parser.skip_spaces();
    if parser.position != parser.text.len() {
        return Err(parser.error("unexpected trailing text"));
    }
    Ok(expression)
}

struct Parser<'a> {
    text: &'a [u8],
    position: usize,
}

impl Parser<'_> {
    fn error(&self, message: &str) -> SymbolError {
        SymbolError::Parse(format!("{message} at offset {}", self.position))
    }

    fn skip_spaces(&mut self) {
        while self.text.get(self.position) == Some(&b' ') {
            self.position += 1;
        }
    }

    fn eat(&mut self, token: &str) -> bool {
        self.skip_spaces();
        if self.text[self.position..].starts_with(token.as_bytes()) {
            self.position += token.len();
            true
        } else {
            false
        }
    }

    fn unary(&mut self) -> Result<Expr> {
        if self.eat("*") {
            return Ok(Expr::Deref(Box::new(self.unary()?)));
        }
        if self.eat("&") {
            return Ok(Expr::AddressOf(Box::new(self.unary()?)));
        }
        self.postfix()
    }

    fn postfix(&mut self) -> Result<Expr> {
        let mut expression = self.primary()?;
        loop {
            if self.eat("->") {
                expression = Expr::Arrow(Box::new(expression), self.identifier()?);
            } else if self.eat(".") {
                expression = Expr::Member(Box::new(expression), self.identifier()?);
            } else if self.eat("[") {
                let index = self.integer()?;
                if !self.eat("]") {
                    return Err(self.error("expected ']'"));
                }
                expression = Expr::Index(Box::new(expression), index);
            } else {
                return Ok(expression);
            }
        }
    }

    fn primary(&mut self) -> Result<Expr> {
        if self.eat("(") {
            let inner = self.unary()?;
            if !self.eat(")") {
                return Err(self.error("expected ')'"));
            }
            return Ok(inner);
        }
        self.path().map(Expr::Path)
    }

    fn identifier(&mut self) -> Result<String> {
        self.skip_spaces();
        let start = self.position;
        while self
            .text
            .get(self.position)
            .is_some_and(|c| c.is_ascii_alphanumeric() || *c == b'_')
        {
            self.position += 1;
        }
        if start == self.position || self.text[start].is_ascii_digit() {
            self.position = start;
            return Err(self.error("expected an identifier"));
        }
        Ok(String::from_utf8_lossy(&self.text[start..self.position]).into_owned())
    }

    /// `a::b<int, 3>::c`, keeping template arguments verbatim.
    fn path(&mut self) -> Result<String> {
        self.skip_spaces();
        let start = self.position;
        loop {
            self.identifier()?;
            self.skip_template_arguments();
            if self.text[self.position..].starts_with(b"::") {
                self.position += 2;
            } else {
                break;
            }
        }
        Ok(String::from_utf8_lossy(&self.text[start..self.position]).into_owned())
    }

    fn skip_template_arguments(&mut self) {
        if self.text.get(self.position) != Some(&b'<') {
            return;
        }
        let mut depth = 0usize;
        for (offset, byte) in self.text[self.position..].iter().enumerate() {
            match byte {
                b'<' => depth += 1,
                b'>' => {
                    depth -= 1;
                    if depth == 0 {
                        self.position += offset + 1;
                        return;
                    }
                }
                _ => {}
            }
        }
    }

    fn integer(&mut self) -> Result<i64> {
        self.skip_spaces();
        let negative = self.eat("-");
        self.skip_spaces();
        let start = self.position;
        let radix = if self.text[self.position..].starts_with(b"0x") {
            self.position += 2;
            16
        } else {
            10
        };
        let digits_start = self.position;
        while self
            .text
            .get(self.position)
            .is_some_and(|c| c.is_ascii_hexdigit())
        {
            self.position += 1;
        }
        let digits = std::str::from_utf8(&self.text[digits_start..self.position]).unwrap_or("");
        let value = i64::from_str_radix(digits, radix).map_err(|_| {
            self.position = start;
            SymbolError::Parse(format!("expected an integer at offset {start}"))
        })?;
        Ok(if negative { -value } else { value })
    }
}

/// An object in target memory.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Place {
    pub addr: u32,
    pub ty: TypeRef,
    /// Array dimensions already consumed by indexing.
    pub consumed: usize,
    pub bits: Option<BitField>,
}

impl Place {
    pub fn new(addr: u32, ty: TypeRef) -> Self {
        Self {
            addr,
            ty,
            consumed: 0,
            bits: None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Evaluated {
    Place(Place),
    /// The result of `&expr`: a pointer value with no storage of its own.
    Address {
        value: u32,
        target: TypeRef,
    },
}

impl SymbolDb {
    fn find_variable(&self, name: &str) -> Result<Variable> {
        let mut found = self.globals(name);
        if found.is_empty() {
            found = self.globals_by_suffix(name);
        }
        found.sort_by_key(|v| v.addr);
        found.dedup_by_key(|v| v.addr);
        match found.len() {
            0 => Err(SymbolError::NotFound(name.to_owned())),
            1 => Ok(found.remove(0)),
            _ => Err(SymbolError::Ambiguous {
                name: name.to_owned(),
                candidates: found
                    .iter()
                    .take(8)
                    .map(|v| format!("{} @ {:#010x}", v.name, v.addr))
                    .collect::<Vec<_>>()
                    .join(", "),
            }),
        }
    }

    pub fn evaluate(&self, expr: &Expr, memory: &mut dyn MemoryReader) -> Result<Evaluated> {
        self.evaluate_with(expr, memory, &[])
    }

    /// Like [`Self::evaluate`], with names in `locals` taking priority.
    pub fn evaluate_with(
        &self,
        expr: &Expr,
        memory: &mut dyn MemoryReader,
        locals: &[(String, Place)],
    ) -> Result<Evaluated> {
        match expr {
            Expr::Path(name) => {
                if let Some((_, place)) = locals.iter().find(|(local, _)| local == name) {
                    return Ok(Evaluated::Place(*place));
                }
                let variable = self.find_variable(name)?;
                let ty = self.variable_type(&variable)?.ok_or_else(|| {
                    SymbolError::Type(format!("{} has no type information", variable.name))
                })?;
                Ok(Evaluated::Place(Place::new(variable.addr, ty)))
            }
            Expr::Member(base, field) => {
                let place = self.evaluate_place(base, memory, locals)?;
                let place = self.through_references(place, memory)?;
                self.member(place, field).map(Evaluated::Place)
            }
            Expr::Arrow(base, field) => {
                let place = self.evaluate_place(base, memory, locals)?;
                let pointee = self.dereference(place, memory)?;
                self.member(pointee, field).map(Evaluated::Place)
            }
            Expr::Index(base, index) => {
                let place = self.evaluate_place(base, memory, locals)?;
                let place = self.through_references(place, memory)?;
                self.index(place, *index, memory).map(Evaluated::Place)
            }
            Expr::Deref(base) => match self.evaluate_with(base, memory, locals)? {
                Evaluated::Address { value, target } => {
                    Ok(Evaluated::Place(Place::new(value, target)))
                }
                Evaluated::Place(place) => self.dereference(place, memory).map(Evaluated::Place),
            },
            Expr::AddressOf(base) => {
                let place = self.evaluate_place(base, memory, locals)?;
                if place.bits.is_some() {
                    return Err(SymbolError::Type(
                        "cannot take the address of a bit-field".to_owned(),
                    ));
                }
                Ok(Evaluated::Address {
                    value: place.addr,
                    target: place.ty,
                })
            }
        }
    }

    fn evaluate_place(
        &self,
        expr: &Expr,
        memory: &mut dyn MemoryReader,
        locals: &[(String, Place)],
    ) -> Result<Place> {
        match self.evaluate_with(expr, memory, locals)? {
            Evaluated::Place(place) => Ok(place),
            Evaluated::Address { .. } => Err(SymbolError::Type(
                "the result of '&' has no storage; dereference it first".to_owned(),
            )),
        }
    }

    fn read_pointer(&self, addr: u32, memory: &mut dyn MemoryReader) -> Result<u32> {
        let mut bytes = [0u8; 4];
        memory.read(addr, &mut bytes).map_err(SymbolError::Memory)?;
        Ok(u32::from_le_bytes(bytes))
    }

    fn through_references(&self, mut place: Place, memory: &mut dyn MemoryReader) -> Result<Place> {
        for _ in 0..4 {
            let (_, info) = self.strip(place.ty)?;
            let TypeKind::Reference(target) = info.kind else {
                return Ok(place);
            };
            let target = target.ok_or_else(|| SymbolError::Type("void reference".to_owned()))?;
            place = Place::new(self.read_pointer(place.addr, memory)?, target);
        }
        Ok(place)
    }

    fn dereference(&self, place: Place, memory: &mut dyn MemoryReader) -> Result<Place> {
        let place = self.through_references(place, memory)?;
        let (_, info) = self.strip(place.ty)?;
        match &info.kind {
            TypeKind::Pointer(Some(target)) => {
                Ok(Place::new(self.read_pointer(place.addr, memory)?, *target))
            }
            TypeKind::Pointer(None) => {
                Err(SymbolError::Type("cannot dereference void*".to_owned()))
            }
            TypeKind::Array { .. } => self.index(place, 0, memory),
            _ => Err(SymbolError::Type(format!(
                "{} is not a pointer",
                self.type_name(place.ty)
            ))),
        }
    }

    fn member(&self, place: Place, field: &str) -> Result<Place> {
        let (offset, member) = self.find_member(place.ty, field)?.ok_or_else(|| {
            SymbolError::Type(format!(
                "{} has no member named {field:?}",
                self.type_name(place.ty)
            ))
        })?;
        let ty = member
            .ty
            .ok_or_else(|| SymbolError::Type(format!("member {field} has no type")))?;
        Ok(Place {
            addr: offset_address(place.addr, offset as i64)?,
            ty,
            consumed: 0,
            bits: member.bits,
        })
    }

    fn index(&self, place: Place, index: i64, memory: &mut dyn MemoryReader) -> Result<Place> {
        let (_, info) = self.strip(place.ty)?;
        match &info.kind {
            TypeKind::Array {
                element: Some(element),
                dimensions,
            } => {
                let remaining = &dimensions[place.consumed..];
                if let Some(Some(count)) = remaining.first() {
                    if index < 0 || index as u64 >= *count {
                        return Err(SymbolError::Type(format!(
                            "index {index} is out of range for {}",
                            self.type_name(place.ty)
                        )));
                    }
                }
                let mut stride = self.size_of(*element, 0)?;
                for dimension in remaining.iter().skip(1) {
                    stride *= dimension.unwrap_or(0);
                }
                let addr = offset_address(place.addr, index * stride as i64)?;
                Ok(if remaining.len() <= 1 {
                    Place::new(addr, *element)
                } else {
                    Place {
                        addr,
                        ty: place.ty,
                        consumed: place.consumed + 1,
                        bits: None,
                    }
                })
            }
            TypeKind::Pointer(Some(target)) => {
                let base = self.read_pointer(place.addr, memory)?;
                let stride = self.size_of(*target, 0)?;
                let addr = offset_address(base, index * stride as i64)?;
                Ok(Place::new(addr, *target))
            }
            _ => Err(SymbolError::Type(format!(
                "cannot index {}",
                self.type_name(place.ty)
            ))),
        }
    }
}

fn offset_address(base: u32, offset: i64) -> Result<u32> {
    u32::try_from(i64::from(base) + offset).map_err(|_| {
        SymbolError::Type(format!(
            "address {base:#x} + {offset} is outside the address space"
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn path(name: &str) -> Box<Expr> {
        Box::new(Expr::Path(name.to_owned()))
    }

    #[test]
    fn parses_chains() {
        assert_eq!(
            parse("a.b[2]").unwrap(),
            Expr::Index(Box::new(Expr::Member(path("a"), "b".to_owned())), 2)
        );
        assert_eq!(
            parse("ns::x->y").unwrap(),
            Expr::Arrow(path("ns::x"), "y".to_owned())
        );
    }

    #[test]
    fn parses_unary_and_parentheses() {
        assert_eq!(parse("*p").unwrap(), Expr::Deref(path("p")));
        assert_eq!(
            parse("&(*p).x").unwrap(),
            Expr::AddressOf(Box::new(Expr::Member(
                Box::new(Expr::Deref(path("p"))),
                "x".to_owned()
            )))
        );
    }

    #[test]
    fn keeps_template_arguments_in_paths() {
        assert_eq!(
            parse("ns::Buf<float, 4>::s_count").unwrap(),
            Expr::Path("ns::Buf<float, 4>::s_count".to_owned())
        );
    }

    #[test]
    fn accepts_hex_and_negative_indices() {
        assert_eq!(parse("a[0x10]").unwrap(), Expr::Index(path("a"), 16));
        assert_eq!(parse("a[-1]").unwrap(), Expr::Index(path("a"), -1));
    }

    #[test]
    fn rejects_malformed_input() {
        for bad in ["", "a.", "a[", "a[]", "a[1", "1a", "a b", "(a", "a.1"] {
            assert!(parse(bad).is_err(), "{bad:?} should not parse");
        }
    }

    #[test]
    fn address_arithmetic_is_checked() {
        assert_eq!(offset_address(0x100, -0x10).unwrap(), 0xF0);
        assert!(offset_address(0x10, -0x20).is_err());
        assert!(offset_address(u32::MAX, 1).is_err());
    }
}
