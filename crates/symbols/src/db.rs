use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use gimli::{AttributeValue, DebugInfoOffset, Dwarf, UnitOffset};
use object::{Object, ObjectSection, ObjectSymbol, SectionKind, SymbolKind};

use crate::cache;
use crate::index::{self, FunctionEntry, Index, RawElfSymbol, R};
use crate::lines::LineTable;
use crate::paths;
use crate::scope::Node;
use crate::types::{Type, TypeRef};
use crate::{MemoryReader, Result, SymbolError};

/// The ELF stays mapped on Linux, where linkers replace their output by
/// unlinking it. Windows cannot write a file while it is mapped, which would
/// break the next link, so there it is read into memory instead.
#[cfg(not(windows))]
type ElfBytes = memmap2::Mmap;
#[cfg(windows)]
type ElfBytes = Vec<u8>;

#[cfg(not(windows))]
fn read_elf(path: &Path) -> std::io::Result<ElfBytes> {
    let file = std::fs::File::open(path)?;
    // SAFETY: the file is opened read-only. Linkers replace their output by
    // unlinking it, so the mapped inode is not modified in place.
    unsafe { memmap2::Mmap::map(&file) }
}

#[cfg(windows)]
fn read_elf(path: &Path) -> std::io::Result<ElfBytes> {
    std::fs::read(path)
}

#[derive(Clone, Debug, Default)]
pub struct LoadOptions {
    /// Index cache location; defaults to `index` under [`paths::cache_root`].
    pub cache_dir: Option<PathBuf>,
    /// Ignore any cached index and do not write a new one.
    pub no_cache: bool,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct LoadStats {
    pub cache_hit: bool,
    pub hash: Duration,
    /// Time spent building or deserializing the index.
    pub index: Duration,
    pub total: Duration,
    pub units: usize,
    pub globals: usize,
    pub functions: usize,
    pub types: usize,
    pub files: usize,
    pub string_bytes: usize,
}

/// A global or static variable.
#[derive(Clone, Debug)]
pub struct Variable {
    pub name: String,
    pub addr: u32,
    /// Size from the ELF symbol table; 0 when unknown.
    pub size: u32,
    pub(crate) unit: u32,
    pub(crate) die: u32,
}

/// Global variable completion candidate for UI autocomplete.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GlobalCompletion {
    pub name: String,
    pub type_name: String,
    pub addr: u32,
}

/// A function with code in the image.
#[derive(Clone, Debug)]
pub struct Function {
    pub name: String,
    pub low: u32,
    pub high: u32,
}

#[derive(Clone, Copy, Debug)]
struct CodeSection {
    addr: u32,
    file_offset: usize,
    len: usize,
}

pub struct SymbolDb {
    // Holds 'static slices into `_map`; see `load`.
    pub(crate) dwarf: Dwarf<R>,
    pub(crate) debug_frame: gimli::DebugFrame<R>,
    pub(crate) index: Index,
    code_sections: Vec<CodeSection>,
    symbols_by_addr: Vec<usize>,
    units: Mutex<HashMap<u32, Arc<gimli::Unit<R>>>>,
    pub(crate) type_cache: Mutex<HashMap<TypeRef, Arc<Type>>>,
    pub(crate) qualified_names: Mutex<HashMap<TypeRef, Option<String>>>,
    pub(crate) line_tables: Mutex<HashMap<u32, Arc<LineTable>>>,
    pub(crate) function_trees: Mutex<HashMap<TypeRef, Arc<Node>>>,
    hash: String,
    stats: LoadStats,
    _map: ElfBytes,
}

impl SymbolDb {
    pub fn load(path: &Path, options: &LoadOptions) -> Result<Self> {
        let started = Instant::now();
        let io_error = |source| SymbolError::Io {
            path: path.display().to_string(),
            source,
        };
        let map = read_elf(path).map_err(io_error)?;

        let hash_started = Instant::now();
        let hash = blake3::Hasher::new()
            .update_rayon(&map)
            .finalize()
            .to_hex()
            .to_string();
        let hash_time = hash_started.elapsed();

        // SAFETY: the slices borrowed from this reference live in `dwarf` and
        // `index` construction only. `_map` is moved into the same struct and
        // never mutated; neither a mapping nor a Vec's heap buffer moves with
        // its owner, so every slice stays valid for the life of the `SymbolDb`.
        // The 'static lifetime is not exposed outside this crate.
        let data: &'static [u8] = unsafe { std::slice::from_raw_parts(map.as_ptr(), map.len()) };

        let object =
            object::File::parse(data).map_err(|error| SymbolError::Object(error.to_string()))?;
        if !object.is_little_endian() {
            return Err(SymbolError::Object(
                "only little-endian targets are supported".to_owned(),
            ));
        }
        let dwarf = Dwarf::load(|id| -> Result<R, gimli::Error> {
            let bytes = object
                .section_by_name(id.name())
                .and_then(|section| section.data().ok())
                .unwrap_or(&[]);
            Ok(gimli::EndianSlice::new(bytes, gimli::LittleEndian))
        })?;

        let frame_bytes = object
            .section_by_name(".debug_frame")
            .and_then(|section| section.data().ok())
            .unwrap_or(&[]);
        let mut debug_frame =
            gimli::DebugFrame::from(gimli::EndianSlice::new(frame_bytes, gimli::LittleEndian));
        debug_frame.set_address_size(4);

        let index_started = Instant::now();
        let cache_file = if options.no_cache {
            None
        } else {
            options
                .cache_dir
                .clone()
                .or_else(cache::default_dir)
                .map(|dir| cache::file_for(&dir, &hash))
        };
        let cached = cache_file.as_deref().and_then(cache::load);
        let cache_hit = cached.is_some();
        let index = match cached {
            Some(index) => index,
            None => {
                let index = index::build(&dwarf, elf_symbols(&object))?;
                if let Some(path) = &cache_file {
                    if let Err(error) = cache::store(path, &index) {
                        tracing::warn!("cannot write index cache: {error}");
                    }
                }
                index
            }
        };
        let index_time = index_started.elapsed();

        let mut code_sections: Vec<CodeSection> = object
            .sections()
            .filter(|section| section.kind() == SectionKind::Text && section.size() > 0)
            .filter_map(|section| {
                let addr = u32::try_from(section.address()).ok()?;
                let (offset, len) = section.file_range()?;
                let file_offset = usize::try_from(offset).ok()?;
                let len = usize::try_from(len).ok()?;
                file_offset
                    .checked_add(len)
                    .filter(|&end| end <= map.len())?;
                Some(CodeSection {
                    addr,
                    file_offset,
                    len,
                })
            })
            .collect();
        code_sections.sort_by_key(|section| section.addr);

        let mut symbols_by_addr: Vec<usize> = (0..index.elf_symbols.len())
            .filter(|&i| index.elf_symbols[i].addr != 0)
            .collect();
        symbols_by_addr.sort_by_key(|&i| {
            let sym = &index.elf_symbols[i];
            (sym.addr, std::cmp::Reverse(sym.size))
        });

        let stats = LoadStats {
            cache_hit,
            hash: hash_time,
            index: index_time,
            total: started.elapsed(),
            units: index.units.len(),
            globals: index.globals.len(),
            functions: index.functions.len(),
            types: index.types.len(),
            files: index.files.len(),
            string_bytes: index.strings.len(),
        };
        Ok(Self {
            dwarf,
            debug_frame,
            index,
            code_sections,
            symbols_by_addr,
            units: Mutex::new(HashMap::new()),
            type_cache: Mutex::new(HashMap::new()),
            qualified_names: Mutex::new(HashMap::new()),
            line_tables: Mutex::new(HashMap::new()),
            function_trees: Mutex::new(HashMap::new()),
            hash,
            stats,
            _map: map,
        })
    }

    pub fn stats(&self) -> LoadStats {
        self.stats
    }

    /// Hex digest of the ELF contents.
    pub fn hash(&self) -> &str {
        &self.hash
    }

    fn make_variable(&self, entry: &index::GlobalEntry) -> Variable {
        Variable {
            name: self.index.str(entry.name).to_owned(),
            addr: entry.addr,
            size: entry.size,
            unit: entry.unit,
            die: entry.die,
        }
    }

    /// Variables with this exact qualified name. Several can exist when
    /// different translation units define file-local statics of the same name.
    pub fn globals(&self, name: &str) -> Vec<Variable> {
        self.index
            .equal_range(&self.index.globals, |e| e.name, name)
            .iter()
            .map(|e| self.make_variable(e))
            .collect()
    }

    /// Variables whose qualified name ends with `::name`.
    pub fn globals_by_suffix(&self, name: &str) -> Vec<Variable> {
        let suffix = format!("::{name}");
        self.index
            .globals
            .iter()
            .filter(|e| self.index.str(e.name).ends_with(&suffix))
            .map(|e| self.make_variable(e))
            .collect()
    }

    pub fn functions(&self, name: &str) -> Vec<Function> {
        self.index
            .equal_range(&self.index.functions, |e| e.name, name)
            .iter()
            .map(|e| Function {
                name: self.index.str(e.name).to_owned(),
                low: e.low,
                high: e.high,
            })
            .collect()
    }

    pub(crate) fn function_entry_at(&self, pc: u32) -> Option<&FunctionEntry> {
        let ranges = &self.index.ranges;
        let position = ranges.partition_point(|r| r.low <= pc);
        let range = ranges.get(position.checked_sub(1)?)?;
        (pc < range.high).then(|| &self.index.functions[range.function as usize])
    }

    pub fn function_at(&self, pc: u32) -> Option<Function> {
        self.function_entry_at(pc).map(|entry| Function {
            name: self.index.str(entry.name).to_owned(),
            low: entry.low,
            high: entry.high,
        })
    }

    /// Every global, for the Globals scope.
    pub fn all_globals(&self) -> Vec<Variable> {
        self.index
            .globals
            .iter()
            .map(|entry| self.make_variable(entry))
            .collect()
    }

    /// Search global/static variables for UI autocomplete, prioritizing prefix matches.
    pub fn complete_globals(&self, query: &str, limit: usize) -> Vec<GlobalCompletion> {
        let needle = query.trim().to_ascii_lowercase();
        let limit = limit.clamp(1, 100);
        let is_internal =
            |name: &str| name.is_empty() || name.starts_with("_GLOBAL__") || name.starts_with('$');

        let mut ranked: Vec<(u8, &index::GlobalEntry, &str)> = Vec::new();
        if needle.is_empty() {
            for entry in &self.index.globals {
                let name = self.index.str(entry.name);
                if is_internal(name) {
                    continue;
                }
                ranked.push((0, entry, name));
                if ranked.len() >= limit {
                    break;
                }
            }
        } else {
            for entry in &self.index.globals {
                let name = self.index.str(entry.name);
                if is_internal(name) {
                    continue;
                }
                let lower = name.to_ascii_lowercase();
                let short = lower.rsplit("::").next().unwrap_or(&lower);
                if lower.starts_with(&needle) || short.starts_with(&needle) {
                    ranked.push((0, entry, name));
                } else if lower.contains(&needle) {
                    ranked.push((1, entry, name));
                }
            }
            ranked.sort_by(|a, b| (a.0, a.2.len(), a.2).cmp(&(b.0, b.2.len(), b.2)));
            ranked.truncate(limit);
        }

        ranked
            .into_iter()
            .map(|(_, entry, name)| {
                let var = self.make_variable(entry);
                let type_name = self
                    .variable_type(&var)
                    .ok()
                    .flatten()
                    .map(|ty| self.type_name(ty))
                    .unwrap_or_default();
                GlobalCompletion {
                    name: name.to_owned(),
                    type_name,
                    addr: entry.addr,
                }
            })
            .collect()
    }

    /// Look up a linker-level symbol, including ones without debug info.
    pub fn elf_symbol(&self, name: &str) -> Option<(u32, u32)> {
        self.index
            .equal_range(&self.index.elf_symbols, |e| e.name, name)
            .first()
            .map(|e| (e.addr, e.size))
    }

    /// Name of the linker symbol covering `addr`.
    pub fn symbol_at(&self, addr: u32) -> Option<String> {
        let pos = self
            .symbols_by_addr
            .partition_point(|&i| self.index.elf_symbols[i].addr <= addr);
        for &idx in self.symbols_by_addr[..pos].iter().rev() {
            let entry = &self.index.elf_symbols[idx];
            if entry.size > 0 && addr - entry.addr < entry.size {
                return Some(self.index.str(entry.name).to_owned());
            }
            if addr - entry.addr > 0x1_0000 {
                break;
            }
        }
        // Handwritten assembly handlers often omit `.size`; allow matching within
        // an executable section up to the next symbol or 256 bytes.
        if let Some(&idx) = pos.checked_sub(1).and_then(|p| self.symbols_by_addr.get(p)) {
            let entry = &self.index.elf_symbols[idx];
            if entry.size == 0 && self.is_code_addr(addr) {
                let next_addr = self.symbols_by_addr[pos..]
                    .iter()
                    .map(|&i| self.index.elf_symbols[i].addr)
                    .find(|&next| next > entry.addr)
                    .unwrap_or(u32::MAX);
                let limit = next_addr.saturating_sub(entry.addr).min(256);
                if addr - entry.addr < limit {
                    return Some(self.index.str(entry.name).to_owned());
                }
            }
        }
        None
    }

    pub(crate) fn is_code_addr(&self, addr: u32) -> bool {
        let pos = self.code_sections.partition_point(|s| s.addr <= addr);
        let Some(section) = pos.checked_sub(1).and_then(|p| self.code_sections.get(p)) else {
            return false;
        };
        usize::try_from(addr - section.addr).is_ok_and(|offset| offset < section.len)
    }

    /// Start address of the function or assembly routine containing `pc`.
    pub(crate) fn function_start_at(&self, pc: u32) -> Option<u32> {
        if let Some(entry) = self.function_entry_at(pc) {
            return Some(entry.low);
        }
        let pos = self
            .symbols_by_addr
            .partition_point(|&i| self.index.elf_symbols[i].addr <= pc);
        for &idx in self.symbols_by_addr[..pos].iter().rev() {
            let entry = &self.index.elf_symbols[idx];
            if entry.size > 0 && pc - entry.addr < entry.size {
                return Some(entry.addr);
            }
            if pc - entry.addr > 0x1_0000 {
                break;
            }
        }
        if let Some(&idx) = pos.checked_sub(1).and_then(|p| self.symbols_by_addr.get(p)) {
            let entry = &self.index.elf_symbols[idx];
            if entry.size == 0 && self.is_code_addr(pc) {
                let next_addr = self.symbols_by_addr[pos..]
                    .iter()
                    .map(|&i| self.index.elf_symbols[i].addr)
                    .find(|&next| next > entry.addr)
                    .unwrap_or(u32::MAX);
                let limit = next_addr.saturating_sub(entry.addr).min(256);
                if pc - entry.addr < limit {
                    return Some(entry.addr);
                }
            }
        }
        None
    }

    /// Read machine code from the mapped ELF `.text` sections, falling back to
    /// target memory when `addr` is not covered by any mapped section.
    pub(crate) fn read_code(
        &self,
        addr: u32,
        buf: &mut [u8],
        memory: &mut dyn MemoryReader,
    ) -> Result<()> {
        if buf.is_empty() {
            return Ok(());
        }
        let pos = self.code_sections.partition_point(|s| s.addr <= addr);
        if let Some(section) = pos.checked_sub(1).and_then(|p| self.code_sections.get(p)) {
            if let Ok(offset) = usize::try_from(addr - section.addr) {
                if offset
                    .checked_add(buf.len())
                    .is_some_and(|end| end <= section.len)
                {
                    let start = section.file_offset + offset;
                    buf.copy_from_slice(&self._map[start..start + buf.len()]);
                    return Ok(());
                }
            }
        }
        memory.read(addr, buf).map_err(SymbolError::Memory)
    }

    /// The definition of a type, given its qualified name.
    pub fn type_by_name(&self, name: &str) -> Option<TypeRef> {
        self.index
            .equal_range(&self.index.types, |e| e.name, name)
            .first()
            .map(|e| TypeRef {
                unit: e.unit,
                die: e.die,
            })
    }

    /// Normalized source paths ending in `path`, for matching what an editor
    /// sends against what the compiler recorded.
    pub fn source_files(&self, path: &str) -> Vec<String> {
        let path = paths::normalize(path);
        let suffix = path.trim_start_matches("./");
        self.index
            .files
            .iter()
            .map(|f| self.index.str(f.path))
            .filter(|candidate| paths::ends_with_components(candidate, suffix))
            .map(str::to_owned)
            .collect()
    }

    pub(crate) fn unit(&self, offset: u32) -> Result<Arc<gimli::Unit<R>>> {
        let mut units = self.units.lock().expect("unit cache lock");
        if let Some(unit) = units.get(&offset) {
            return Ok(unit.clone());
        }
        let header = self.dwarf.unit_header(DebugInfoOffset(offset as usize))?;
        let unit = Arc::new(self.dwarf.unit(header)?);
        units.insert(offset, unit.clone());
        Ok(unit)
    }

    /// Turn a reference attribute into a [`TypeRef`], whichever unit it is in.
    pub(crate) fn resolve_ref(&self, unit: u32, value: AttributeValue<R>) -> Option<TypeRef> {
        match value {
            AttributeValue::UnitRef(UnitOffset(die)) => Some(TypeRef {
                unit,
                die: die as u32,
            }),
            AttributeValue::DebugInfoRef(DebugInfoOffset(global)) => {
                let global = global as u32;
                let position = self.index.units.partition_point(|u| *u <= global);
                let unit = *self.index.units.get(position.checked_sub(1)?)?;
                Some(TypeRef {
                    unit,
                    die: global - unit,
                })
            }
            _ => None,
        }
    }

    /// Type of a variable, looking through DW_AT_specification.
    pub fn variable_type(&self, variable: &Variable) -> Result<Option<TypeRef>> {
        let mut current = TypeRef {
            unit: variable.unit,
            die: variable.die,
        };
        for _ in 0..8 {
            let unit = self.unit(current.unit)?;
            let entry = unit
                .unit_ref(&self.dwarf)
                .entry(UnitOffset(current.die as usize))?;
            if let Some(value) = entry.attr_value(gimli::DW_AT_type) {
                return Ok(self.resolve_ref(current.unit, value));
            }
            let next = entry
                .attr_value(gimli::DW_AT_specification)
                .or_else(|| entry.attr_value(gimli::DW_AT_abstract_origin));
            match next.and_then(|value| self.resolve_ref(current.unit, value)) {
                Some(target) => current = target,
                None => return Ok(None),
            }
        }
        Ok(None)
    }
}

fn elf_symbols(object: &object::File<'static>) -> Vec<RawElfSymbol> {
    object
        .symbols()
        .filter(|symbol| symbol.is_definition())
        .filter_map(|symbol| {
            let name = symbol.name().ok()?;
            if name.is_empty()
                || name == "$t"
                || name == "$d"
                || name == "$a"
                || name.starts_with("$t.")
                || name.starts_with("$d.")
                || name.starts_with("$a.")
            {
                return None;
            }
            // Thumb function symbols carry the mode in bit 0.
            let mut addr = symbol.address() as u32;
            if symbol.kind() == SymbolKind::Text {
                addr &= !1;
            }
            Some(RawElfSymbol {
                name: name.to_owned(),
                addr,
                size: symbol.size() as u32,
            })
        })
        .collect()
}
