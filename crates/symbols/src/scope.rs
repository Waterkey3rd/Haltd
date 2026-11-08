//! Lexical structure of functions: inlined instances and their variables.

use std::sync::Arc;

use gimli::{constants as dw, AttributeValue, UnitOffset};

use crate::db::SymbolDb;
use crate::index::R;
use crate::lines::{LineTable, SourceLocation};
use crate::types::TypeRef;
use crate::Result;

const MAX_ORIGIN_CHAIN: usize = 8;

/// A local variable or parameter at some scope.
#[derive(Clone, Debug)]
pub struct LocalVar {
    pub name: String,
    pub ty: Option<TypeRef>,
    pub is_param: bool,
    pub(crate) unit: u32,
    pub(crate) location: Option<AttributeValue<R>>,
    pub(crate) const_value: Option<AttributeValue<R>>,
    pub(crate) frame_base: Option<AttributeValue<R>>,
}

/// One function or inlined instance active at a PC.
#[derive(Clone, Debug)]
pub struct ScopeFrame {
    pub name: String,
    pub is_inlined: bool,
    /// DIE offset, identifying this instance.
    pub die: u32,
    pub ranges: Vec<(u32, u32)>,
    pub entry_pc: u32,
    /// For inlined instances, where the call was written.
    pub call_site: Option<SourceLocation>,
    /// Innermost block first, so a shadowing name appears before the shadowed.
    pub vars: Vec<LocalVar>,
}

#[derive(Debug)]
pub(crate) struct Node {
    is_block: bool,
    frame: ScopeFrame,
    children: Vec<Node>,
}

fn contains(ranges: &[(u32, u32)], pc: u32) -> bool {
    ranges.iter().any(|(low, high)| pc >= *low && pc < *high)
}

impl Node {
    /// Extend `chain` (outermost first) with whatever is active at `pc`.
    fn collect(&self, pc: u32, chain: &mut Vec<ScopeFrame>) {
        for child in &self.children {
            if !contains(&child.frame.ranges, pc) {
                continue;
            }
            if child.is_block {
                child.collect(pc, chain);
            } else {
                chain.push(child.frame.clone());
                child.collect(pc, chain);
            }
            break;
        }
    }

    /// Variables of nested blocks active at `pc`, innermost first.
    fn block_vars(&self, pc: u32, out: &mut Vec<LocalVar>) {
        for child in &self.children {
            if child.is_block && contains(&child.frame.ranges, pc) {
                child.block_vars(pc, out);
                out.extend(child.frame.vars.iter().cloned());
                break;
            }
        }
    }

    /// The node for chain position `depth`, descending through blocks.
    fn add_block_vars_along(&self, pc: u32, chain: &mut [ScopeFrame]) {
        let mut vars = Vec::new();
        self.block_vars(pc, &mut vars);
        if let Some(first) = chain.first_mut() {
            vars.append(&mut first.vars);
            first.vars = vars;
        }
        let mut node = self;
        loop {
            let next = node
                .children
                .iter()
                .find(|child| contains(&child.frame.ranges, pc));
            match next {
                Some(child) if child.is_block => node = child,
                Some(child) => {
                    child.add_block_vars_along(pc, &mut chain[1..]);
                    return;
                }
                None => return,
            }
        }
    }
}

impl SymbolDb {
    fn entry_ranges(
        &self,
        unit: &gimli::Unit<R>,
        entry: &gimli::DebuggingInformationEntry<R>,
    ) -> Result<Vec<(u32, u32)>> {
        let mut iter = self.dwarf.die_ranges(unit, entry)?;
        let mut ranges = Vec::new();
        while let Some(range) = iter.next()? {
            if range.begin != 0 && range.end > range.begin {
                ranges.push((range.begin as u32, range.end as u32));
            }
        }
        Ok(ranges)
    }

    /// An attribute of this DIE, or of the abstract DIE it instantiates.
    fn attr_through_origin(
        &self,
        unit_offset: u32,
        entry: &gimli::DebuggingInformationEntry<R>,
        name: dw::DwAt,
    ) -> Result<Option<(u32, AttributeValue<R>)>> {
        if let Some(value) = entry.attr_value(name) {
            return Ok(Some((unit_offset, value)));
        }
        let mut origin = entry
            .attr_value(dw::DW_AT_abstract_origin)
            .or_else(|| entry.attr_value(dw::DW_AT_specification))
            .and_then(|value| self.resolve_ref(unit_offset, value));
        for _ in 0..MAX_ORIGIN_CHAIN {
            let Some(target) = origin else {
                return Ok(None);
            };
            let unit = self.unit(target.unit)?;
            let entry = unit
                .unit_ref(&self.dwarf)
                .entry(UnitOffset(target.die as usize))?;
            if let Some(value) = entry.attr_value(name) {
                return Ok(Some((target.unit, value)));
            }
            origin = entry
                .attr_value(dw::DW_AT_abstract_origin)
                .or_else(|| entry.attr_value(dw::DW_AT_specification))
                .and_then(|value| self.resolve_ref(target.unit, value));
        }
        Ok(None)
    }

    fn name_through_origin(
        &self,
        unit_offset: u32,
        entry: &gimli::DebuggingInformationEntry<R>,
    ) -> Result<Option<String>> {
        let Some((unit, value)) = self.attr_through_origin(unit_offset, entry, dw::DW_AT_name)?
        else {
            return Ok(None);
        };
        let unit = self.unit(unit)?;
        let text = self.dwarf.attr_string(&unit, value)?;
        Ok(Some(String::from_utf8_lossy(text.slice()).into_owned()))
    }

    fn parse_var(
        &self,
        unit_offset: u32,
        entry: &gimli::DebuggingInformationEntry<R>,
        frame_base: Option<AttributeValue<R>>,
    ) -> Result<Option<LocalVar>> {
        let Some(name) = self.name_through_origin(unit_offset, entry)? else {
            return Ok(None);
        };
        let ty = self
            .attr_through_origin(unit_offset, entry, dw::DW_AT_type)?
            .and_then(|(unit, value)| self.resolve_ref(unit, value));
        Ok(Some(LocalVar {
            name,
            ty,
            is_param: entry.tag() == dw::DW_TAG_formal_parameter,
            unit: unit_offset,
            location: entry.attr_value(dw::DW_AT_location),
            const_value: entry.attr_value(dw::DW_AT_const_value),
            frame_base,
        }))
    }

    fn parse_children(
        &self,
        unit_offset: u32,
        unit: &gimli::Unit<R>,
        table: &LineTable,
        tree: gimli::EntriesTreeNode<'_, '_, R>,
        frame_base: Option<AttributeValue<R>>,
        parent: &mut Node,
    ) -> Result<()> {
        let mut children = tree.children();
        while let Some(child) = children.next()? {
            let entry = child.entry();
            match entry.tag() {
                dw::DW_TAG_variable | dw::DW_TAG_formal_parameter => {
                    if let Some(var) = self.parse_var(unit_offset, entry, frame_base)? {
                        parent.frame.vars.push(var);
                    }
                }
                dw::DW_TAG_lexical_block | dw::DW_TAG_inlined_subroutine => {
                    let is_block = entry.tag() == dw::DW_TAG_lexical_block;
                    let ranges = self.entry_ranges(unit, entry)?;
                    let entry_pc = match entry.attr_value(dw::DW_AT_entry_pc) {
                        Some(AttributeValue::Addr(address)) => address as u32,
                        _ => ranges.iter().map(|r| r.0).min().unwrap_or(0),
                    };
                    let call_line = entry
                        .attr_value(dw::DW_AT_call_line)
                        .and_then(|value| value.udata_value());
                    let call_file = match entry.attr_value(dw::DW_AT_call_file) {
                        Some(AttributeValue::FileIndex(file)) => Some(file),
                        Some(other) => other.udata_value(),
                        None => None,
                    };
                    let call_site = match (call_file, call_line) {
                        (Some(file), Some(line)) => {
                            table.files.get(file as usize).map(|file| SourceLocation {
                                file: file.clone(),
                                line: line as u32,
                            })
                        }
                        _ => None,
                    };
                    let name = if is_block {
                        String::new()
                    } else {
                        self.name_through_origin(unit_offset, entry)?
                            .unwrap_or_else(|| "<inlined>".to_owned())
                    };
                    let mut node = Node {
                        is_block,
                        frame: ScopeFrame {
                            name,
                            is_inlined: !is_block,
                            die: entry.offset().0 as u32,
                            ranges,
                            entry_pc,
                            call_site,
                            vars: Vec::new(),
                        },
                        children: Vec::new(),
                    };
                    self.parse_children(unit_offset, unit, table, child, frame_base, &mut node)?;
                    parent.children.push(node);
                }
                _ => {}
            }
        }
        Ok(())
    }

    fn function_tree(&self, unit_offset: u32, die: u32, name: &str) -> Result<Arc<Node>> {
        let key = TypeRef {
            unit: unit_offset,
            die,
        };
        if let Some(found) = self
            .function_trees
            .lock()
            .expect("function tree lock")
            .get(&key)
        {
            return Ok(found.clone());
        }
        let unit = self.unit(unit_offset)?;
        let table = self.line_table(unit_offset)?;
        let unit_ref = unit.unit_ref(&self.dwarf);
        let mut tree = unit_ref.entries_tree(Some(UnitOffset(die as usize)))?;
        let root = tree.root()?;
        let entry = root.entry();
        let frame_base = entry.attr_value(dw::DW_AT_frame_base);
        let ranges = self.entry_ranges(&unit, entry)?;
        let entry_pc = ranges.iter().map(|r| r.0).min().unwrap_or(0);
        let mut node = Node {
            is_block: false,
            frame: ScopeFrame {
                name: name.to_owned(),
                is_inlined: false,
                die,
                ranges,
                entry_pc,
                call_site: None,
                vars: Vec::new(),
            },
            children: Vec::new(),
        };
        self.parse_children(unit_offset, &unit, &table, root, frame_base, &mut node)?;
        let node = Arc::new(node);
        self.function_trees
            .lock()
            .expect("function tree lock")
            .insert(key, node.clone());
        Ok(node)
    }

    /// The function and inlined instances active at `pc`, outermost first.
    /// Empty for code without debug info.
    pub fn scope_at(&self, pc: u32) -> Result<Vec<ScopeFrame>> {
        let Some(entry) = self.function_entry_at(pc) else {
            return Ok(Vec::new());
        };
        let (unit, die) = (entry.unit, entry.die);
        let name = self.index.str(entry.name).to_owned();
        let tree = self.function_tree(unit, die, &name)?;
        let mut chain = vec![tree.frame.clone()];
        tree.collect(pc, &mut chain);
        tree.add_block_vars_along(pc, &mut chain);
        Ok(chain)
    }
}
