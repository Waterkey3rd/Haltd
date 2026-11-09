//! Lua rules for displaying template-heavy types (DESIGN §5.7).
//!
//! Rules run with no direct target access: each value is an integer handle
//! into a table on the Rust side, valid for one [`Visualizers::apply`] call.

use std::cell::RefCell;
use std::path::Path;

use mlua::{Function, Lua, Table, Value};
use symbols::{FormatOptions, MemoryReader, Place, SymbolDb, TypeKind, VirtualMembers};

const PRELUDE: &str = include_str!("prelude.lua");
const BUILTIN: &str = include_str!("builtin.lua");
const NESTED_RENDER: FormatOptions = FormatOptions {
    max_elements: 8,
    max_depth: 1,
};

/// What rules had to say about a value. `None` fields use the default display.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Visualized {
    pub summary: Option<String>,
    pub children: Option<Vec<(String, Place)>>,
}

pub struct Visualizers {
    lua: Lua,
}

/// Lets an expression name what the rules show, so that a member seen in Live
/// Watch can be watched or plotted by itself.
impl VirtualMembers for Visualizers {
    fn member(
        &self,
        db: &SymbolDb,
        place: Place,
        name: &str,
        memory: &mut dyn MemoryReader,
    ) -> Option<Place> {
        self.run(db, memory, place, true)?
            .children?
            .into_iter()
            .find(|(child, _)| child == name)
            .map(|(_, child)| child)
    }
}

struct Context<'a> {
    db: &'a SymbolDb,
    memory: &'a mut dyn MemoryReader,
    places: Vec<Place>,
}

impl Context<'_> {
    fn add(&mut self, place: Place) -> usize {
        self.places.push(place);
        self.places.len() - 1
    }

    fn pointer_value(&mut self, place: Place) -> Option<u32> {
        let mut bytes = [0u8; 4];
        self.memory.read(place.addr, &mut bytes).ok()?;
        Some(u32::from_le_bytes(bytes))
    }

    fn pointee(&self, place: Place) -> Option<symbols::TypeRef> {
        let (_, info) = self.db.strip(place.ty).ok()?;
        match info.kind {
            TypeKind::Pointer(target) | TypeKind::Reference(target) => target,
            _ => None,
        }
    }

    fn field(&mut self, handle: usize, name: &str) -> Option<usize> {
        let place = *self.places.get(handle)?;
        let (offset, member) = self.db.find_member(place.ty, name).ok()??;
        Some(self.add(Place {
            addr: place.addr + offset as u32,
            ty: member.ty?,
            consumed: 0,
            bits: member.bits,
        }))
    }

    /// Numeric value of a scalar: integer, enum, bool, float or pointer.
    fn number(&mut self, handle: usize) -> Option<f64> {
        let place = *self.places.get(handle)?;
        let (_, info) = self.db.strip(place.ty).ok()?;
        let size = self.db.place_size(&place).ok()? as usize;
        if size == 0 || size > 8 {
            return None;
        }
        let mut bytes = [0u8; 8];
        self.memory.read(place.addr, &mut bytes[..size]).ok()?;
        let raw = u64::from_le_bytes(bytes);
        Some(match &info.kind {
            TypeKind::Base(symbols::BaseEncoding::Float) if size == 4 => {
                f64::from(f32::from_bits(raw as u32))
            }
            TypeKind::Base(symbols::BaseEncoding::Float) if size == 8 => f64::from_bits(raw),
            TypeKind::Base(symbols::BaseEncoding::Signed | symbols::BaseEncoding::SignedChar) => {
                let shift = 64 - size as u32 * 8;
                (((raw << shift) as i64) >> shift) as f64
            }
            _ => raw as f64,
        })
    }

    fn at(&mut self, handle: usize, index: i64) -> Option<usize> {
        let place = *self.places.get(handle)?;
        let target = self.pointee(place)?;
        let base = self.pointer_value(place)?;
        let stride = self.db.size_of(target, 0).ok()? as i64;
        let addr = u32::try_from(i64::from(base) + index * stride).ok()?;
        Some(self.add(Place::new(addr, target)))
    }
}

impl Visualizers {
    pub fn with_builtin() -> mlua::Result<Self> {
        let lua = Lua::new();
        lua.load(PRELUDE).set_name("prelude").exec()?;
        lua.load(BUILTIN).set_name("builtin").exec()?;
        Ok(Self { lua })
    }

    /// Add a project's rules; they take priority over what is already loaded.
    pub fn load_file(&self, path: &Path) -> mlua::Result<()> {
        let source = std::fs::read_to_string(path).map_err(mlua::Error::external)?;
        self.lua
            .load(source)
            .set_name(path.display().to_string())
            .exec()
    }

    /// Run the first rule matching this value's type, if any.
    pub fn apply(
        &self,
        db: &SymbolDb,
        memory: &mut dyn MemoryReader,
        place: Place,
    ) -> Option<Visualized> {
        self.run(db, memory, place, false)
    }

    fn run(
        &self,
        db: &SymbolDb,
        memory: &mut dyn MemoryReader,
        place: Place,
        resolving: bool,
    ) -> Option<Visualized> {
        let (resolved, info) = db.strip(place.ty).ok()?;
        if !matches!(info.kind, TypeKind::Struct { .. }) || place.bits.is_some() {
            return None;
        }
        let type_name = db.qualified_type_name(resolved).ok()??;
        match self.call(db, memory, place, &type_name, resolving) {
            Ok(result) => result,
            Err(error) => {
                tracing::debug!("visualizer for {type_name} failed: {error}");
                None
            }
        }
    }

    fn call(
        &self,
        db: &SymbolDb,
        memory: &mut dyn MemoryReader,
        place: Place,
        type_name: &str,
        resolving: bool,
    ) -> mlua::Result<Option<Visualized>> {
        let context = RefCell::new(Context {
            db,
            memory,
            places: vec![place],
        });
        let lua = &self.lua;
        lua.scope(|scope| {
            let host = lua.create_table()?;
            let c = &context;
            host.set(
                "field",
                scope.create_function(move |_, (h, name): (usize, String)| {
                    Ok(c.borrow_mut().field(h, &name))
                })?,
            )?;
            host.set(
                "num",
                scope.create_function(move |_, h: usize| Ok(c.borrow_mut().number(h)))?,
            )?;
            host.set(
                "str",
                scope.create_function(move |_, h: usize| {
                    let mut guard = c.borrow_mut();
                    let context = &mut *guard;
                    let place = context.places[h];
                    Ok(context
                        .db
                        .render_place(&place, context.memory, &NESTED_RENDER)
                        .unwrap_or_else(|error| format!("<{error}>")))
                })?,
            )?;
            host.set(
                "type",
                scope.create_function(move |_, h: usize| {
                    let guard = c.borrow();
                    Ok(guard.db.type_name(guard.places[h].ty))
                })?,
            )?;
            host.set(
                "deref",
                scope.create_function(move |_, h: usize| Ok(c.borrow_mut().at(h, 0)))?,
            )?;
            host.set(
                "at",
                scope
                    .create_function(move |_, (h, i): (usize, i64)| Ok(c.borrow_mut().at(h, i)))?,
            )?;
            host.set(
                "pointee_size",
                scope.create_function(move |_, h: usize| {
                    let guard = c.borrow();
                    let place = guard.places[h];
                    Ok(guard
                        .pointee(place)
                        .and_then(|target| guard.db.size_of(target, 0).ok()))
                })?,
            )?;
            host.set(
                "cstring",
                scope.create_function(move |_, (h, length): (usize, usize)| {
                    let mut guard = c.borrow_mut();
                    let place = guard.places[h];
                    let Some(pointer) = guard.pointer_value(place) else {
                        return Ok(None);
                    };
                    let mut bytes = vec![0u8; length];
                    if guard.memory.read(pointer, &mut bytes).is_err() {
                        return Ok(None);
                    }
                    Ok(Some(String::from_utf8_lossy(&bytes).into_owned()))
                })?,
            )?;
            host.set(
                "elements",
                scope.create_function(move |_, h: usize| {
                    let mut guard = c.borrow_mut();
                    let context = &mut *guard;
                    let place = context.places[h];
                    let children = context
                        .db
                        .children(&place, context.memory)
                        .map_err(mlua::Error::external)?;
                    Ok(children
                        .into_iter()
                        .map(|(_, child)| context.add(child))
                        .collect::<Vec<usize>>())
                })?,
            )?;
            lua.globals().set("__host", host)?;

            let apply: Function = lua.globals().get("__jlinkd_apply")?;
            let (summary, children): (Option<String>, Option<Table>) = match apply
                .call::<mlua::MultiValue>((type_name, 0usize, resolving))?
                .into_vec()
                .as_slice()
            {
                [] | [Value::Nil] => return Ok(None),
                [summary] => (lua.unpack(summary.clone())?, None),
                [summary, children, ..] => {
                    (lua.unpack(summary.clone())?, lua.unpack(children.clone())?)
                }
            };
            let children = match children {
                None => None,
                Some(table) => {
                    let guard = context.borrow();
                    let mut out = Vec::new();
                    for entry in table.sequence_values::<Table>() {
                        let entry = entry?;
                        let handle: usize = entry.get("h")?;
                        out.push((entry.get::<String>("name")?, guard.places[handle]));
                    }
                    Some(out)
                }
            };
            Ok(Some(Visualized { summary, children }))
        })
    }
}
