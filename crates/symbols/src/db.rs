use std::collections::HashMap;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use gimli::{AttributeValue, DebugInfoOffset, Dwarf, UnitOffset};
use memmap2::Mmap;
use object::{Object, ObjectSection, ObjectSymbol, SymbolKind};

use crate::cache;
use crate::index::{self, FunctionEntry, Index, RawElfSymbol, R};
use crate::lines::LineTable;
use crate::scope::Node;
use crate::types::{Type, TypeRef};
use crate::{Result, SymbolError};

#[derive(Clone, Debug, Default)]
pub struct LoadOptions {
    /// Index cache location; defaults to `~/.cache/jlinkd/index`.
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

/// A function with code in the image.
#[derive(Clone, Debug)]
pub struct Function {
    pub name: String,
    pub low: u32,
    pub high: u32,
}

pub struct SymbolDb {
    // Holds 'static slices into `_map`; see `load`.
    pub(crate) dwarf: Dwarf<R>,
    pub(crate) debug_frame: gimli::DebugFrame<R>,
    pub(crate) index: Index,
    units: Mutex<HashMap<u32, Arc<gimli::Unit<R>>>>,
    pub(crate) type_cache: Mutex<HashMap<TypeRef, Arc<Type>>>,
    pub(crate) qualified_names: Mutex<HashMap<TypeRef, Option<String>>>,
    pub(crate) line_tables: Mutex<HashMap<u32, Arc<LineTable>>>,
    pub(crate) function_trees: Mutex<HashMap<TypeRef, Arc<Node>>>,
    hash: String,
    stats: LoadStats,
    _map: Mmap,
}

impl SymbolDb {
    pub fn load(path: &Path, options: &LoadOptions) -> Result<Self> {
        let started = Instant::now();
        let io_error = |source| SymbolError::Io {
            path: path.display().to_string(),
            source,
        };
        let file = File::open(path).map_err(io_error)?;
        // SAFETY: the file is opened read-only. Linkers replace their output
        // by unlinking it, so the mapped inode is not modified in place.
        let map = unsafe { Mmap::map(&file) }.map_err(io_error)?;

        let hash_started = Instant::now();
        let hash = blake3::Hasher::new()
            .update_rayon(&map)
            .finalize()
            .to_hex()
            .to_string();
        let hash_time = hash_started.elapsed();

        // SAFETY: the slices borrowed from this reference live in `dwarf` and
        // `index` construction only. `_map` is moved into the same struct, is
        // never mutated, and its heap-independent mapping does not move, so
        // every slice stays valid for the life of the `SymbolDb`. The 'static
        // lifetime is not exposed outside this crate.
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

    /// Look up a linker-level symbol, including ones without debug info.
    pub fn elf_symbol(&self, name: &str) -> Option<(u32, u32)> {
        self.index
            .equal_range(&self.index.elf_symbols, |e| e.name, name)
            .first()
            .map(|e| (e.addr, e.size))
    }

    /// Name of the linker symbol covering `addr`.
    pub fn symbol_at(&self, addr: u32) -> Option<String> {
        self.index
            .elf_symbols
            .iter()
            .find(|e| addr >= e.addr && addr - e.addr < e.size)
            .map(|e| self.index.str(e.name).to_owned())
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
        let suffix = path.trim_start_matches("./");
        self.index
            .files
            .iter()
            .map(|f| self.index.str(f.path))
            .filter(|candidate| {
                *candidate == suffix
                    || candidate
                        .strip_suffix(suffix)
                        .is_some_and(|rest| rest.ends_with('/'))
            })
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
            if name.is_empty() {
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
