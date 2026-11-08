//! Parallel construction of the name index (DESIGN §5.3.1).
//!
//! Each compilation unit is scanned with gimli's raw entry reader. Only DIEs
//! that can name something are decoded; the rest are skipped by abbreviation.

use std::collections::HashMap;

use gimli::constants as dw;
use gimli::{AttributeValue, Dwarf, UnitHeader};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};

pub(crate) type R = gimli::EndianSlice<'static, gimli::LittleEndian>;

/// Parent index meaning "no enclosing named scope".
const NONE: u32 = u32::MAX;
/// Guards against reference cycles in malformed DWARF.
const MAX_CHAIN: usize = 64;

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
pub(crate) struct StrRef {
    offset: u32,
    len: u32,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub(crate) struct GlobalEntry {
    pub name: StrRef,
    pub unit: u32,
    pub die: u32,
    pub addr: u32,
    pub size: u32,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub(crate) struct FunctionEntry {
    pub name: StrRef,
    pub unit: u32,
    pub die: u32,
    pub low: u32,
    pub high: u32,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub(crate) struct AddressRange {
    pub low: u32,
    pub high: u32,
    pub function: u32,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub(crate) struct TypeEntry {
    pub name: StrRef,
    pub unit: u32,
    pub die: u32,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub(crate) struct FileEntry {
    pub path: StrRef,
    pub units_start: u32,
    pub units_len: u32,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub(crate) struct ElfSymbol {
    pub name: StrRef,
    pub addr: u32,
    pub size: u32,
}

/// The persistent index: flat tables over one string blob, each sorted by
/// name. This keeps deserialization of a large index to a few memcpy-like
/// passes.
#[derive(Debug, Default, Serialize, Deserialize)]
pub(crate) struct Index {
    #[serde(with = "serde_bytes")]
    pub strings: Vec<u8>,
    /// Section offsets of all units, ascending.
    pub units: Vec<u32>,
    pub globals: Vec<GlobalEntry>,
    pub functions: Vec<FunctionEntry>,
    /// Sorted by `low`.
    pub ranges: Vec<AddressRange>,
    pub types: Vec<TypeEntry>,
    pub files: Vec<FileEntry>,
    pub file_units: Vec<u32>,
    pub elf_symbols: Vec<ElfSymbol>,
}

impl Index {
    pub fn str(&self, r: StrRef) -> &str {
        let bytes = &self.strings[r.offset as usize..(r.offset + r.len) as usize];
        // The blob is assembled from `String`s only.
        std::str::from_utf8(bytes).unwrap_or("")
    }

    /// The entries of a name-sorted table whose name equals `name`.
    pub fn equal_range<'a, T>(
        &self,
        table: &'a [T],
        key: impl Fn(&T) -> StrRef,
        name: &str,
    ) -> &'a [T] {
        let start = table.partition_point(|e| self.str(key(e)) < name);
        let len = table[start..].partition_point(|e| self.str(key(e)) == name);
        &table[start..start + len]
    }
}

struct Node {
    offset: u32,
    parent: u32,
    name: Option<&'static [u8]>,
    /// DW_AT_specification or DW_AT_abstract_origin within this unit.
    origin: Option<u32>,
}

/// Follow specification links to the DIE that determines the name and scope.
/// Returns the first name seen and the parent of the end of the chain.
fn resolve(nodes: &[Node], index: usize) -> (Option<&'static [u8]>, u32) {
    let mut node = &nodes[index];
    let mut name = node.name;
    for _ in 0..MAX_CHAIN {
        let Some(origin) = node.origin else { break };
        let Ok(next) = nodes.binary_search_by_key(&origin, |n| n.offset) else {
            break;
        };
        node = &nodes[next];
        name = name.or(node.name);
    }
    (name, node.parent)
}

fn write_qualified(nodes: &[Node], index: usize, out: &mut String, depth: usize) {
    if depth > MAX_CHAIN {
        return;
    }
    let (name, parent) = resolve(nodes, index);
    if parent != NONE {
        write_qualified(nodes, parent as usize, out, depth + 1);
    }
    // Anonymous namespaces and unnamed scopes contribute nothing, so
    // `ns::(anonymous)::x` can be written as `ns::x`.
    if let Some(name) = name {
        if !out.is_empty() {
            out.push_str("::");
        }
        out.push_str(&String::from_utf8_lossy(name));
    }
}

fn qualified(nodes: &[Node], index: usize) -> Option<String> {
    resolve(nodes, index).0?;
    let mut out = String::new();
    write_qualified(nodes, index, &mut out, 0);
    Some(out)
}

/// `(low, high)` address pairs.
type CodeRanges = Vec<(u32, u32)>;

#[derive(Default)]
struct UnitScan {
    unit: u32,
    globals: Vec<(String, u32, u32)>,
    functions: Vec<(String, u32, CodeRanges)>,
    types: Vec<(String, u32)>,
    files: Vec<String>,
}

#[derive(Default)]
struct Attrs {
    name: Option<&'static [u8]>,
    origin: Option<u32>,
    declaration: bool,
    low: Option<u32>,
    high_addr: Option<u32>,
    high_offset: Option<u32>,
    ranges: Option<AttributeValue<R>>,
    address: Option<u32>,
}

/// Extract the address from a location that is exactly `DW_OP_addr X`.
fn static_address(value: AttributeValue<R>) -> Option<u32> {
    const DW_OP_ADDR: u8 = 0x03;
    let AttributeValue::Exprloc(expression) = value else {
        return None;
    };
    match expression.0.slice() {
        [DW_OP_ADDR, a, b, c, d] => Some(u32::from_le_bytes([*a, *b, *c, *d])),
        _ => None,
    }
}

fn is_name_bearing(tag: dw::DwTag) -> bool {
    matches!(
        tag,
        dw::DW_TAG_namespace
            | dw::DW_TAG_structure_type
            | dw::DW_TAG_class_type
            | dw::DW_TAG_union_type
            | dw::DW_TAG_enumeration_type
            | dw::DW_TAG_typedef
            | dw::DW_TAG_base_type
            | dw::DW_TAG_subprogram
            | dw::DW_TAG_variable
    )
}

fn is_type(tag: dw::DwTag) -> bool {
    matches!(
        tag,
        dw::DW_TAG_structure_type
            | dw::DW_TAG_class_type
            | dw::DW_TAG_union_type
            | dw::DW_TAG_enumeration_type
            | dw::DW_TAG_typedef
            | dw::DW_TAG_base_type
    )
}

/// Pending items recorded while walking; names are built once all nodes exist,
/// since a specification may refer forward.
struct Walk {
    nodes: Vec<Node>,
    variables: Vec<(u32, u32, u32)>,
    subprograms: Vec<(u32, u32, CodeRanges)>,
    types: Vec<(u32, u32)>,
}

fn walk_unit(dwarf: &Dwarf<R>, unit: &gimli::Unit<R>) -> gimli::Result<Walk> {
    let mut walk = Walk {
        nodes: Vec::new(),
        variables: Vec::new(),
        subprograms: Vec::new(),
        types: Vec::new(),
    };
    let mut scopes: Vec<u32> = Vec::with_capacity(32);
    let mut raw = unit.entries_raw(None)?;
    while !raw.is_empty() {
        let depth = raw.next_depth() as usize;
        let offset = raw.next_offset().0 as u32;
        let Some(abbrev) = raw.read_abbreviation()? else {
            continue;
        };
        if scopes.len() <= depth {
            scopes.resize(depth + 1, NONE);
        }
        let parent = if depth == 0 { NONE } else { scopes[depth - 1] };
        let tag = abbrev.tag();
        if !is_name_bearing(tag) {
            raw.skip_attributes(abbrev.attributes())?;
            // Lexical blocks and the like are transparent for naming.
            scopes[depth] = parent;
            continue;
        }

        let mut attrs = Attrs::default();
        for spec in abbrev.attributes() {
            let attr = raw.read_attribute_inline(*spec)?;
            match attr.name() {
                dw::DW_AT_name => {
                    attrs.name = Some(dwarf.attr_string(unit, attr.value())?.slice());
                }
                dw::DW_AT_specification | dw::DW_AT_abstract_origin => {
                    if let AttributeValue::UnitRef(target) = attr.value() {
                        attrs.origin = Some(target.0 as u32);
                    }
                }
                dw::DW_AT_declaration => attrs.declaration = true,
                dw::DW_AT_low_pc => {
                    if let AttributeValue::Addr(address) = attr.value() {
                        attrs.low = Some(address as u32);
                    }
                }
                dw::DW_AT_high_pc => match attr.value() {
                    AttributeValue::Addr(address) => attrs.high_addr = Some(address as u32),
                    other => attrs.high_offset = other.udata_value().map(|v| v as u32),
                },
                dw::DW_AT_ranges => attrs.ranges = Some(attr.value()),
                dw::DW_AT_location if tag == dw::DW_TAG_variable => {
                    attrs.address = static_address(attr.value());
                }
                _ => {}
            }
        }

        let node = walk.nodes.len() as u32;
        walk.nodes.push(Node {
            offset,
            parent,
            name: attrs.name,
            origin: attrs.origin,
        });
        scopes[depth] = node;

        if attrs.declaration {
            continue;
        }
        match tag {
            dw::DW_TAG_variable => {
                // The linker zeroes the address of discarded objects.
                if let Some(address) = attrs.address.filter(|a| *a != 0) {
                    walk.variables.push((node, offset, address));
                }
            }
            dw::DW_TAG_subprogram => {
                let ranges = code_ranges(dwarf, unit, &attrs)?;
                if !ranges.is_empty() {
                    walk.subprograms.push((node, offset, ranges));
                }
            }
            _ if is_type(tag) => walk.types.push((node, offset)),
            _ => {}
        }
    }
    Ok(walk)
}

fn code_ranges(
    dwarf: &Dwarf<R>,
    unit: &gimli::Unit<R>,
    attrs: &Attrs,
) -> gimli::Result<Vec<(u32, u32)>> {
    let mut ranges = Vec::new();
    if let Some(value) = attrs.ranges {
        if let Some(mut iter) = dwarf.attr_ranges(unit, value)? {
            while let Some(range) = iter.next()? {
                ranges.push((range.begin as u32, range.end as u32));
            }
        }
    } else if let Some(low) = attrs.low {
        let high = attrs
            .high_addr
            .or_else(|| attrs.high_offset.map(|size| low.wrapping_add(size)));
        if let Some(high) = high {
            ranges.push((low, high));
        }
    }
    // A low address of zero marks code discarded by --gc-sections.
    ranges.retain(|(low, high)| *low != 0 && high > low);
    Ok(ranges)
}

fn unit_files(dwarf: &Dwarf<R>, unit: &gimli::Unit<R>) -> gimli::Result<Vec<String>> {
    let Some(program) = unit.line_program.as_ref() else {
        return Ok(Vec::new());
    };
    let header = program.header();
    let comp_dir = unit
        .comp_dir
        .map(|d| String::from_utf8_lossy(d.slice()).into_owned());
    let mut files = Vec::with_capacity(header.file_names().len());
    for file in header.file_names() {
        let name = dwarf.attr_string(unit, file.path_name())?;
        let directory = match file.directory(header) {
            Some(value) => Some(dwarf.attr_string(unit, value)?),
            None => None,
        };
        files.push(join_path(
            comp_dir.as_deref(),
            directory
                .map(|d| String::from_utf8_lossy(d.slice()))
                .as_deref(),
            &String::from_utf8_lossy(name.slice()),
        ));
    }
    files.sort_unstable();
    files.dedup();
    Ok(files)
}

/// Combine the parts of a line-table path and remove `.` and `..` components.
pub(crate) fn join_path(comp_dir: Option<&str>, directory: Option<&str>, name: &str) -> String {
    let mut full = String::new();
    for part in [comp_dir, directory, Some(name)].into_iter().flatten() {
        if part.starts_with('/') {
            full.clear();
        } else if !full.is_empty() && !full.ends_with('/') {
            full.push('/');
        }
        full.push_str(part);
    }
    let absolute = full.starts_with('/');
    let mut parts: Vec<&str> = Vec::new();
    for component in full.split('/') {
        match component {
            "" | "." => {}
            ".." => {
                if parts.last().is_some_and(|p| *p != "..") {
                    parts.pop();
                } else if !absolute {
                    parts.push("..");
                }
            }
            other => parts.push(other),
        }
    }
    let joined = parts.join("/");
    if absolute {
        format!("/{joined}")
    } else {
        joined
    }
}

fn scan_unit(dwarf: &Dwarf<R>, header: UnitHeader<R>) -> gimli::Result<UnitScan> {
    let unit_offset = header.offset().0 as u32;
    let unit = dwarf.unit(header)?;
    let walk = walk_unit(dwarf, &unit)?;
    let name_of = |node: u32| qualified(&walk.nodes, node as usize);

    let mut scan = UnitScan {
        unit: unit_offset,
        files: unit_files(dwarf, &unit)?,
        ..UnitScan::default()
    };
    for (node, die, address) in walk.variables.iter().copied() {
        if let Some(name) = name_of(node) {
            scan.globals.push((name, die, address));
        }
    }
    for (node, die, ranges) in walk.subprograms {
        if let Some(name) = name_of(node) {
            scan.functions.push((name, die, ranges));
        }
    }
    for (node, die) in walk.types.iter().copied() {
        if let Some(name) = name_of(node) {
            scan.types.push((name, die));
        }
    }
    Ok(scan)
}

/// Qualified name of a DIE, found by rescanning its unit. Only used for the
/// rare forward declaration that has to be matched to a definition.
pub(crate) fn qualified_name(
    dwarf: &Dwarf<R>,
    unit: &gimli::Unit<R>,
    die: u32,
) -> gimli::Result<Option<String>> {
    let walk = walk_unit(dwarf, unit)?;
    Ok(walk
        .nodes
        .binary_search_by_key(&die, |n| n.offset)
        .ok()
        .and_then(|index| qualified(&walk.nodes, index)))
}

/// Appends strings to the blob, sharing the bytes of consecutive duplicates.
#[derive(Default)]
struct Blob {
    bytes: Vec<u8>,
    last: StrRef,
    has_last: bool,
}

impl Blob {
    fn add(&mut self, text: &str) -> StrRef {
        if self.has_last {
            let previous = self.last;
            let start = previous.offset as usize;
            if self.bytes[start..start + previous.len as usize] == *text.as_bytes() {
                return previous;
            }
        }
        let reference = StrRef {
            offset: self.bytes.len() as u32,
            len: text.len() as u32,
        };
        self.bytes.extend_from_slice(text.as_bytes());
        self.last = reference;
        self.has_last = true;
        reference
    }
}

pub(crate) struct RawElfSymbol {
    pub name: String,
    pub addr: u32,
    pub size: u32,
}

pub(crate) fn build(dwarf: &Dwarf<R>, mut elf_symbols: Vec<RawElfSymbol>) -> gimli::Result<Index> {
    let mut headers = Vec::new();
    let mut iter = dwarf.units();
    while let Some(header) = iter.next()? {
        headers.push(header);
    }

    // One bad unit should cost its own symbols, not the whole debug session.
    let scans: Vec<UnitScan> = headers
        .into_par_iter()
        .filter_map(|header| match scan_unit(dwarf, header) {
            Ok(scan) => Some(scan),
            Err(error) => {
                tracing::warn!("skipping a compilation unit: {error}");
                None
            }
        })
        .collect();

    let mut units: Vec<u32> = scans.iter().map(|s| s.unit).collect();
    units.sort_unstable();

    let sizes: HashMap<u32, u32> = elf_symbols.iter().map(|s| (s.addr, s.size)).collect();

    let mut globals = Vec::new();
    let mut functions = Vec::new();
    let mut types = Vec::new();
    let mut files: Vec<(String, u32)> = Vec::new();
    for scan in scans {
        let unit = scan.unit;
        globals.extend(scan.globals.into_iter().map(|(n, d, a)| (n, unit, d, a)));
        functions.extend(scan.functions.into_iter().map(|(n, d, r)| (n, unit, d, r)));
        types.extend(scan.types.into_iter().map(|(n, d)| (n, unit, d)));
        files.extend(scan.files.into_iter().map(|f| (f, unit)));
    }

    // Unit and DIE offsets are part of the key so the result is deterministic.
    globals.par_sort_unstable();
    globals.dedup_by(|b, a| a.0 == b.0 && a.3 == b.3);
    functions.par_sort_unstable_by(|a, b| {
        (&a.0, a.3.first(), a.1, a.2).cmp(&(&b.0, b.3.first(), b.1, b.2))
    });
    functions.dedup_by(|b, a| a.0 == b.0 && a.3 == b.3);
    types.par_sort_unstable();
    types.dedup_by(|b, a| a.0 == b.0);
    files.par_sort_unstable();
    files.dedup();
    elf_symbols.par_sort_unstable_by(|a, b| (&a.name, a.addr).cmp(&(&b.name, b.addr)));

    let mut index = Index {
        units,
        ..Index::default()
    };
    let mut blob = Blob::default();

    index.globals = globals
        .iter()
        .map(|(name, unit, die, addr)| GlobalEntry {
            name: blob.add(name),
            unit: *unit,
            die: *die,
            addr: *addr,
            size: sizes.get(addr).copied().unwrap_or(0),
        })
        .collect();

    for (position, (name, unit, die, ranges)) in functions.iter().enumerate() {
        index.functions.push(FunctionEntry {
            name: blob.add(name),
            unit: *unit,
            die: *die,
            low: ranges.iter().map(|r| r.0).min().unwrap_or(0),
            high: ranges.iter().map(|r| r.1).max().unwrap_or(0),
        });
        index
            .ranges
            .extend(ranges.iter().map(|&(low, high)| AddressRange {
                low,
                high,
                function: position as u32,
            }));
    }
    index.ranges.par_sort_unstable_by_key(|r| r.low);

    index.types = types
        .iter()
        .map(|(name, unit, die)| TypeEntry {
            name: blob.add(name),
            unit: *unit,
            die: *die,
        })
        .collect();

    let mut position = 0;
    while position < files.len() {
        let path = &files[position].0;
        let units_start = index.file_units.len() as u32;
        let mut end = position;
        while end < files.len() && files[end].0 == *path {
            index.file_units.push(files[end].1);
            end += 1;
        }
        index.files.push(FileEntry {
            path: blob.add(path),
            units_start,
            units_len: (end - position) as u32,
        });
        position = end;
    }

    index.elf_symbols = elf_symbols
        .iter()
        .map(|s| ElfSymbol {
            name: blob.add(&s.name),
            addr: s.addr,
            size: s.size,
        })
        .collect();

    index.strings = blob.bytes;
    Ok(index)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_are_joined_and_normalized() {
        assert_eq!(
            join_path(Some("/w/build"), Some("../../Core/Inc"), "a.h"),
            "/Core/Inc/a.h"
        );
        assert_eq!(
            join_path(Some("/w/build"), Some("/usr/include/"), "b.h"),
            "/usr/include/b.h"
        );
        assert_eq!(join_path(Some("/w"), None, "./x/../y.c"), "/w/y.c");
        assert_eq!(join_path(None, Some("../a"), "b.c"), "../a/b.c");
    }

    fn node(offset: u32, parent: u32, name: Option<&'static str>, origin: Option<u32>) -> Node {
        Node {
            offset,
            parent,
            name: name.map(str::as_bytes),
            origin,
        }
    }

    #[test]
    fn specification_supplies_name_and_scope() {
        // namespace ns { struct C { static int v; }; } int ns::C::v;
        let nodes = [
            node(10, NONE, Some("ns"), None),
            node(20, 0, Some("C"), None),
            node(30, 1, Some("v"), None),
            node(40, NONE, None, Some(30)),
        ];
        assert_eq!(qualified(&nodes, 3).as_deref(), Some("ns::C::v"));
    }

    #[test]
    fn anonymous_scopes_are_omitted() {
        let nodes = [
            node(10, NONE, Some("ns"), None),
            node(20, 0, None, None),
            node(30, 1, Some("x"), None),
        ];
        assert_eq!(qualified(&nodes, 2).as_deref(), Some("ns::x"));
        assert_eq!(qualified(&nodes, 1), None);
    }

    #[test]
    fn reference_cycles_terminate() {
        let nodes = [
            node(10, NONE, Some("a"), Some(20)),
            node(20, NONE, None, Some(10)),
        ];
        assert_eq!(qualified(&nodes, 0).as_deref(), Some("a"));
    }

    #[test]
    fn blob_shares_consecutive_duplicates_only() {
        let mut blob = Blob::default();
        let a = blob.add("abc");
        let b = blob.add("abc");
        let c = blob.add("xy");
        assert_eq!((a.offset, b.offset), (0, 0));
        assert_eq!(c.offset, 3);
        assert_eq!(blob.bytes.len(), 5);
    }
}
