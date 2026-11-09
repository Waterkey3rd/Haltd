//! Parser and evaluator for variable path expressions such as
//! `ns::g_state.items[2].gain` or `*g_ptr->next`.
//!
//! Conditions can also use C arithmetic, comparisons, logic and casts to
//! scalar types. Integers are computed without wrapping, so `count - 1` is -1
//! when an unsigned `count` is 0; write a cast to get the C result.

use crate::db::{SymbolDb, Variable};
use crate::scalar::{Number, Scalar};
use crate::types::{BitField, TypeKind, TypeRef};
use crate::{MemoryReader, Result, SymbolError};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnaryOp {
    Negate,
    Not,
    BitNot,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BinaryOp {
    Or,
    And,
    BitOr,
    BitXor,
    BitAnd,
    Equal,
    NotEqual,
    Less,
    LessEqual,
    Greater,
    GreaterEqual,
    ShiftLeft,
    ShiftRight,
    Add,
    Subtract,
    Multiply,
    Divide,
    Remainder,
}

/// Longest spellings first, so `<=` is not read as `<`.
const BINARY_OPERATORS: [(&str, BinaryOp, u8); 18] = [
    ("||", BinaryOp::Or, 1),
    ("&&", BinaryOp::And, 2),
    ("==", BinaryOp::Equal, 6),
    ("!=", BinaryOp::NotEqual, 6),
    ("<=", BinaryOp::LessEqual, 7),
    (">=", BinaryOp::GreaterEqual, 7),
    ("<<", BinaryOp::ShiftLeft, 8),
    (">>", BinaryOp::ShiftRight, 8),
    ("|", BinaryOp::BitOr, 3),
    ("^", BinaryOp::BitXor, 4),
    ("&", BinaryOp::BitAnd, 5),
    ("<", BinaryOp::Less, 7),
    (">", BinaryOp::Greater, 7),
    ("+", BinaryOp::Add, 9),
    ("-", BinaryOp::Subtract, 9),
    ("*", BinaryOp::Multiply, 10),
    ("/", BinaryOp::Divide, 10),
    ("%", BinaryOp::Remainder, 10),
];

/// A scalar type named in a cast.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CastType {
    Bool,
    Float,
    Double,
    Integer { bits: u32, signed: bool },
}

fn cast_type(words: &str) -> Option<CastType> {
    let integer = |bits, signed| Some(CastType::Integer { bits, signed });
    match words {
        "bool" | "_Bool" => Some(CastType::Bool),
        "float" => Some(CastType::Float),
        "double" => Some(CastType::Double),
        // Plain char is unsigned on ARM.
        "char" | "unsigned char" | "uint8_t" => integer(8, false),
        "signed char" | "int8_t" => integer(8, true),
        "short" | "short int" | "int16_t" => integer(16, true),
        "unsigned short" | "uint16_t" => integer(16, false),
        "int" | "long" | "int32_t" => integer(32, true),
        "unsigned" | "unsigned int" | "unsigned long" | "uint32_t" => integer(32, false),
        "long long" | "int64_t" => integer(64, true),
        "unsigned long long" | "uint64_t" => integer(64, false),
        _ => None,
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Expr {
    Integer(i128),
    Float(f64),
    Unary(UnaryOp, Box<Expr>),
    Binary(BinaryOp, Box<Expr>, Box<Expr>),
    Cast(CastType, Box<Expr>),
    /// `a[i]` where the index is not a literal.
    IndexBy(Box<Expr>, Box<Expr>),
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
    let expression = parser.expression(0)?;
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

    fn peek_binary(&mut self) -> Option<(&'static str, BinaryOp, u8)> {
        self.skip_spaces();
        BINARY_OPERATORS
            .into_iter()
            .find(|(token, _, _)| self.text[self.position..].starts_with(token.as_bytes()))
    }

    /// Precedence climbing: every operator here is left-associative.
    fn expression(&mut self, minimum: u8) -> Result<Expr> {
        let mut left = self.unary()?;
        while let Some((token, operator, precedence)) = self.peek_binary() {
            if precedence <= minimum {
                break;
            }
            self.position += token.len();
            let right = self.expression(precedence)?;
            left = Expr::Binary(operator, Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    /// `(unsigned int)` and the like, leaving the position alone otherwise.
    fn try_cast(&mut self) -> Option<CastType> {
        let start = self.position;
        if self.eat("(") {
            let mut words = Vec::new();
            while let Ok(word) = self.identifier() {
                words.push(word);
            }
            if self.eat(")") {
                if let Some(cast) = cast_type(&words.join(" ")) {
                    return Some(cast);
                }
            }
        }
        self.position = start;
        None
    }

    fn unary(&mut self) -> Result<Expr> {
        if let Some(cast) = self.try_cast() {
            return Ok(Expr::Cast(cast, Box::new(self.unary()?)));
        }
        if self.eat("!") {
            return Ok(Expr::Unary(UnaryOp::Not, Box::new(self.unary()?)));
        }
        if self.eat("~") {
            return Ok(Expr::Unary(UnaryOp::BitNot, Box::new(self.unary()?)));
        }
        if self.eat("-") {
            return Ok(match self.unary()? {
                Expr::Integer(value) => Expr::Integer(-value),
                Expr::Float(value) => Expr::Float(-value),
                other => Expr::Unary(UnaryOp::Negate, Box::new(other)),
            });
        }
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
                let index = self.expression(0)?;
                if !self.eat("]") {
                    return Err(self.error("expected ']'"));
                }
                expression = match index {
                    Expr::Integer(value) => {
                        let value = i64::try_from(value)
                            .map_err(|_| self.error("the index is out of range"))?;
                        Expr::Index(Box::new(expression), value)
                    }
                    other => Expr::IndexBy(Box::new(expression), Box::new(other)),
                };
            } else {
                return Ok(expression);
            }
        }
    }

    fn primary(&mut self) -> Result<Expr> {
        if self.eat("(") {
            let inner = self.expression(0)?;
            if !self.eat(")") {
                return Err(self.error("expected ')'"));
            }
            return Ok(inner);
        }
        self.skip_spaces();
        if self.text.get(self.position).is_some_and(u8::is_ascii_digit) {
            return self.number();
        }
        let path = self.path()?;
        Ok(match path.as_str() {
            "true" => Expr::Integer(1),
            "false" => Expr::Integer(0),
            _ => Expr::Path(path),
        })
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

    /// Only `<...>` followed by `::` or the end of the text is a template;
    /// anything else is a comparison such as `a<3 && b>2`.
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
                        let end = self.position + offset + 1;
                        let rest = &self.text[end..];
                        if rest.is_empty() || rest.starts_with(b"::") {
                            self.position = end;
                        }
                        return;
                    }
                }
                _ => {}
            }
        }
    }

    /// Decimal, `0x` or `0b` integers with C suffixes, and floats.
    fn number(&mut self) -> Result<Expr> {
        let start = self.position;
        let rest = &self.text[start..];
        let radix = match rest.get(..2) {
            Some(b"0x" | b"0X") => 16,
            Some(b"0b" | b"0B") => 2,
            _ => 10,
        };
        if radix != 10 {
            self.position += 2;
        }
        let digits_start = self.position;
        while self
            .text
            .get(self.position)
            .is_some_and(|c| char::from(*c).is_digit(radix))
        {
            self.position += 1;
        }
        let mut is_float = false;
        if radix == 10 {
            if self.text.get(self.position) == Some(&b'.') {
                is_float = true;
                self.position += 1;
                self.skip_digits();
            }
            if matches!(self.text.get(self.position), Some(b'e' | b'E')) {
                is_float = true;
                self.position += 1;
                if matches!(self.text.get(self.position), Some(b'+' | b'-')) {
                    self.position += 1;
                }
                self.skip_digits();
            }
        }
        let digits = String::from_utf8_lossy(&self.text[digits_start..self.position]).into_owned();
        let parsed = if is_float {
            digits.parse().ok().map(Expr::Float)
        } else {
            i128::from_str_radix(&digits, radix).ok().map(Expr::Integer)
        };
        let suffixes: &[u8] = if is_float { b"fFlL" } else { b"uUlL" };
        while self
            .text
            .get(self.position)
            .is_some_and(|c| suffixes.contains(c))
        {
            self.position += 1;
        }
        let glued = self
            .text
            .get(self.position)
            .is_some_and(|c| c.is_ascii_alphanumeric() || *c == b'_');
        match parsed {
            Some(number) if !glued => Ok(number),
            _ => {
                self.position = start;
                Err(self.error("expected a number"))
            }
        }
    }

    fn skip_digits(&mut self) {
        while self.text.get(self.position).is_some_and(u8::is_ascii_digit) {
            self.position += 1;
        }
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

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Evaluated {
    Place(Place),
    /// A computed number with no storage.
    Value(Number),
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
            Expr::Integer(value) => Ok(Evaluated::Value(Number::Int(*value))),
            Expr::Float(value) => Ok(Evaluated::Value(Number::Float(*value))),
            Expr::Unary(operator, inner) => {
                let value = self.evaluate_number(inner, memory, locals)?;
                apply_unary(*operator, value).map(Evaluated::Value)
            }
            Expr::Binary(operator, left, right) => {
                let left = self.evaluate_number(left, memory, locals)?;
                // The right side may only be safe to read if the left allows it.
                match (operator, left.is_true()) {
                    (BinaryOp::And, false) => return Ok(Evaluated::Value(Number::Int(0))),
                    (BinaryOp::Or, true) => return Ok(Evaluated::Value(Number::Int(1))),
                    _ => {}
                }
                let right = self.evaluate_number(right, memory, locals)?;
                apply_binary(*operator, left, right).map(Evaluated::Value)
            }
            Expr::Cast(cast, inner) => {
                let value = self.evaluate_number(inner, memory, locals)?;
                Ok(Evaluated::Value(apply_cast(*cast, value)))
            }
            Expr::IndexBy(base, index) => {
                let Number::Int(index) = self.evaluate_number(index, memory, locals)? else {
                    return Err(SymbolError::Type("an index must be an integer".to_owned()));
                };
                let index = i64::try_from(index)
                    .map_err(|_| SymbolError::Type("the index is out of range".to_owned()))?;
                let place = self.evaluate_place(base, memory, locals)?;
                let place = self.through_references(place, memory)?;
                self.index(place, index, memory).map(Evaluated::Place)
            }
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
                Evaluated::Value(_) => Err(SymbolError::Type(
                    "cannot dereference a computed value".to_owned(),
                )),
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
            Evaluated::Value(_) => Err(SymbolError::Type(
                "a computed value has no storage".to_owned(),
            )),
        }
    }

    /// The numeric value of an expression, for arithmetic and conditions.
    pub fn evaluate_number(
        &self,
        expr: &Expr,
        memory: &mut dyn MemoryReader,
        locals: &[(String, Place)],
    ) -> Result<Number> {
        match self.evaluate_with(expr, memory, locals)? {
            Evaluated::Value(number) => Ok(number),
            Evaluated::Address { value, .. } => Ok(Number::Int(i128::from(value))),
            Evaluated::Place(place) => self.read_number(place, memory),
        }
    }

    fn read_number(&self, place: Place, memory: &mut dyn MemoryReader) -> Result<Number> {
        let place = self.through_references(place, memory)?;
        // An array stands for the address of its first element.
        if matches!(self.strip(place.ty)?.1.kind, TypeKind::Array { .. }) {
            return Ok(Number::Int(i128::from(place.addr)));
        }
        let scalar = Scalar::of(self, &place).ok_or_else(|| {
            SymbolError::Type(format!("{} is not a number", self.type_name(place.ty)))
        })?;
        let mut bytes = [0u8; 8];
        memory
            .read(place.addr, &mut bytes[..scalar.size as usize])
            .map_err(SymbolError::Memory)?;
        Ok(scalar.number(&bytes))
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

fn apply_unary(operator: UnaryOp, value: Number) -> Result<Number> {
    Ok(match (operator, value) {
        (UnaryOp::Not, value) => Number::Int(i128::from(!value.is_true())),
        (UnaryOp::Negate, Number::Int(value)) => Number::Int(-value),
        (UnaryOp::Negate, Number::Float(value)) => Number::Float(-value),
        (UnaryOp::BitNot, Number::Int(value)) => Number::Int(!value),
        (UnaryOp::BitNot, Number::Float(_)) => {
            return Err(SymbolError::Type("'~' needs an integer".to_owned()))
        }
    })
}

fn apply_binary(operator: BinaryOp, left: Number, right: Number) -> Result<Number> {
    use BinaryOp::*;
    let truth = |value: bool| Number::Int(i128::from(value));
    if let (Number::Int(a), Number::Int(b)) = (left, right) {
        return Ok(match operator {
            Or | And => truth(b != 0),
            Equal => truth(a == b),
            NotEqual => truth(a != b),
            Less => truth(a < b),
            LessEqual => truth(a <= b),
            Greater => truth(a > b),
            GreaterEqual => truth(a >= b),
            BitOr => Number::Int(a | b),
            BitXor => Number::Int(a ^ b),
            BitAnd => Number::Int(a & b),
            Add => Number::Int(a + b),
            Subtract => Number::Int(a - b),
            Multiply => Number::Int(a.saturating_mul(b)),
            Divide | Remainder if b == 0 => {
                return Err(SymbolError::Type("division by zero".to_owned()))
            }
            Divide => Number::Int(a / b),
            Remainder => Number::Int(a % b),
            ShiftLeft | ShiftRight if !(0..64).contains(&b) => {
                return Err(SymbolError::Type(format!("cannot shift by {b}")))
            }
            ShiftLeft => Number::Int(a << b),
            ShiftRight => Number::Int(a >> b),
        });
    }
    let (a, b) = (left.as_f64(), right.as_f64());
    Ok(match operator {
        Or | And => truth(b != 0.0),
        Equal => truth(a == b),
        NotEqual => truth(a != b),
        Less => truth(a < b),
        LessEqual => truth(a <= b),
        Greater => truth(a > b),
        GreaterEqual => truth(a >= b),
        Add => Number::Float(a + b),
        Subtract => Number::Float(a - b),
        Multiply => Number::Float(a * b),
        Divide => Number::Float(a / b),
        BitOr | BitXor | BitAnd | Remainder | ShiftLeft | ShiftRight => {
            return Err(SymbolError::Type("this operator needs integers".to_owned()))
        }
    })
}

fn apply_cast(cast: CastType, value: Number) -> Number {
    match cast {
        CastType::Bool => Number::Int(i128::from(value.is_true())),
        CastType::Float => Number::Float(f64::from(value.as_f64() as f32)),
        CastType::Double => Number::Float(value.as_f64()),
        CastType::Integer { bits, signed } => {
            let whole = match value {
                Number::Int(value) => value,
                Number::Float(value) => value as i128,
            };
            let truncated = whole & ((1i128 << bits) - 1);
            let negative = signed && truncated >> (bits - 1) != 0;
            Number::Int(if negative {
                truncated - (1i128 << bits)
            } else {
                truncated
            })
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
    fn operators_follow_c_precedence() {
        let parsed = parse("a + 2 * 3 == 7 && !b").unwrap();
        let sum = Expr::Binary(
            BinaryOp::Add,
            path("a"),
            Box::new(Expr::Binary(
                BinaryOp::Multiply,
                Box::new(Expr::Integer(2)),
                Box::new(Expr::Integer(3)),
            )),
        );
        let equal = Expr::Binary(BinaryOp::Equal, Box::new(sum), Box::new(Expr::Integer(7)));
        let not = Expr::Unary(UnaryOp::Not, path("b"));
        assert_eq!(
            parsed,
            Expr::Binary(BinaryOp::And, Box::new(equal), Box::new(not))
        );
    }

    #[test]
    fn comparisons_are_not_mistaken_for_templates() {
        assert_eq!(
            parse("a<3").unwrap(),
            Expr::Binary(BinaryOp::Less, path("a"), Box::new(Expr::Integer(3)))
        );
        assert!(matches!(
            parse("a<3 && b>2").unwrap(),
            Expr::Binary(BinaryOp::And, _, _)
        ));
    }

    #[test]
    fn subtraction_and_arrows_are_told_apart() {
        assert_eq!(
            parse("a-1").unwrap(),
            Expr::Binary(BinaryOp::Subtract, path("a"), Box::new(Expr::Integer(1)))
        );
        assert_eq!(
            parse("a->b").unwrap(),
            Expr::Arrow(path("a"), "b".to_owned())
        );
    }

    #[test]
    fn numbers_take_c_forms() {
        assert_eq!(parse("0b101").unwrap(), Expr::Integer(5));
        assert_eq!(parse("10u").unwrap(), Expr::Integer(10));
        assert_eq!(parse("1.5f").unwrap(), Expr::Float(1.5));
        assert_eq!(parse("true").unwrap(), Expr::Integer(1));
    }

    #[test]
    fn casts_need_a_known_scalar_type() {
        assert_eq!(
            parse("(uint8_t)a").unwrap(),
            Expr::Cast(
                CastType::Integer {
                    bits: 8,
                    signed: false
                },
                path("a")
            )
        );
        assert_eq!(parse("(a)").unwrap(), Expr::Path("a".to_owned()));
    }

    #[test]
    fn variable_indices_parse() {
        assert_eq!(parse("a[i]").unwrap(), Expr::IndexBy(path("a"), path("i")));
    }

    fn compute(text: &str) -> Number {
        fn run(expr: &Expr) -> Result<Number> {
            Ok(match expr {
                Expr::Integer(value) => Number::Int(*value),
                Expr::Float(value) => Number::Float(*value),
                Expr::Unary(operator, inner) => apply_unary(*operator, run(inner)?)?,
                Expr::Binary(operator, left, right) => {
                    apply_binary(*operator, run(left)?, run(right)?)?
                }
                Expr::Cast(cast, inner) => apply_cast(*cast, run(inner)?),
                other => panic!("not constant: {other:?}"),
            })
        }
        run(&parse(text).unwrap()).unwrap()
    }

    #[test]
    fn integer_arithmetic_matches_c() {
        assert_eq!(compute("7 / 2"), Number::Int(3));
        assert_eq!(compute("-7 % 3"), Number::Int(-1));
        assert_eq!(compute("1 << 4 | 1"), Number::Int(17));
        assert_eq!(compute("(uint8_t)300"), Number::Int(44));
        assert_eq!(compute("(int8_t)200"), Number::Int(-56));
        assert_eq!(compute("(uint32_t)-1"), Number::Int(0xFFFF_FFFF));
    }

    #[test]
    fn floats_mix_with_integers() {
        assert_eq!(compute("1 / 2.0"), Number::Float(0.5));
        assert_eq!(compute("1.5 > 1"), Number::Int(1));
        assert_eq!(compute("(int)2.9"), Number::Int(2));
    }

    #[test]
    fn invalid_arithmetic_is_an_error() {
        assert!(apply_binary(BinaryOp::Divide, Number::Int(1), Number::Int(0)).is_err());
        assert!(apply_binary(BinaryOp::BitAnd, Number::Float(1.0), Number::Int(1)).is_err());
        assert!(apply_binary(BinaryOp::ShiftLeft, Number::Int(1), Number::Int(200)).is_err());
    }

    #[test]
    fn address_arithmetic_is_checked() {
        assert_eq!(offset_address(0x100, -0x10).unwrap(), 0xF0);
        assert!(offset_address(0x10, -0x20).is_err());
        assert!(offset_address(u32::MAX, 1).is_err());
    }
}
