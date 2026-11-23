//! Line tables, parsed per compilation unit on first use (DESIGN §5.3.2).

use std::collections::BTreeSet;
use std::sync::Arc;

use crate::db::SymbolDb;
use crate::index::join_path;
use crate::{paths, Result};

/// How far a breakpoint may slide forward to the next line with code.
const MAX_LINE_SLIDE: u32 = 10;

#[derive(Clone, Copy, Debug)]
pub(crate) struct Row {
    pub addr: u32,
    pub file: u32,
    pub line: u32,
    pub is_stmt: bool,
    pub end_sequence: bool,
}

#[derive(Debug)]
pub(crate) struct LineTable {
    /// Indexed by the file number in rows and DW_AT_call_file.
    pub files: Vec<String>,
    /// Sorted by sequence; rows within a sequence are in address order.
    pub rows: Vec<Row>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct SourceLocation {
    pub file: String,
    pub line: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedLine {
    /// Where the code really is; may be after the requested line.
    pub file: String,
    pub line: u32,
    pub addresses: Vec<u32>,
}

/// Row covering `pc`, ignoring end-of-sequence markers.
fn row_index(rows: &[Row], pc: u32) -> Option<usize> {
    let position = rows.partition_point(|row| row.addr <= pc);
    let mut index = position.checked_sub(1)?;
    if rows[index].end_sequence {
        return None;
    }
    // Several rows can share an address; the last one describes the code.
    while index + 1 < rows.len() && rows[index + 1].addr == rows[index].addr {
        if rows[index + 1].end_sequence {
            break;
        }
        index += 1;
    }
    Some(index)
}

/// Longest run of identical trailing path components.
fn common_suffix(a: &str, b: &str) -> usize {
    a.rsplit('/')
        .zip(b.rsplit('/'))
        .take_while(|(x, y)| paths::same(x, y))
        .count()
}

impl SymbolDb {
    pub(crate) fn line_table(&self, unit_offset: u32) -> Result<Arc<LineTable>> {
        if let Some(found) = self
            .line_tables
            .lock()
            .expect("line table lock")
            .get(&unit_offset)
        {
            return Ok(found.clone());
        }
        let table = Arc::new(self.parse_line_table(unit_offset)?);
        self.line_tables
            .lock()
            .expect("line table lock")
            .insert(unit_offset, table.clone());
        Ok(table)
    }

    fn parse_line_table(&self, unit_offset: u32) -> Result<LineTable> {
        let unit = self.unit(unit_offset)?;
        let Some(program) = unit.line_program.clone() else {
            return Ok(LineTable {
                files: Vec::new(),
                rows: Vec::new(),
            });
        };
        let header = program.header();
        let base: u64 = if header.version() >= 5 { 0 } else { 1 };
        let comp_dir = unit
            .comp_dir
            .map(|d| String::from_utf8_lossy(d.slice()).into_owned());
        let mut files = Vec::with_capacity(header.file_names().len() + base as usize);
        // DWARF 4 numbers files from 1; keep indices directly usable.
        if base == 1 {
            files.push(String::new());
        }
        for file in header.file_names() {
            let name = self.dwarf.attr_string(&unit, file.path_name())?;
            let directory = match file.directory(header) {
                Some(value) => Some(self.dwarf.attr_string(&unit, value)?),
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

        let mut sequences: Vec<Vec<Row>> = Vec::new();
        let mut current: Vec<Row> = Vec::new();
        let mut rows = program.rows();
        while let Some((_, row)) = rows.next_row()? {
            current.push(Row {
                addr: row.address() as u32,
                file: row.file_index() as u32,
                line: row.line().map_or(0, |line| line.get() as u32),
                is_stmt: row.is_stmt(),
                end_sequence: row.end_sequence(),
            });
            if row.end_sequence() {
                sequences.push(std::mem::take(&mut current));
            }
        }
        // A start address of zero marks code removed by --gc-sections.
        sequences.retain(|sequence| sequence.first().is_some_and(|row| row.addr != 0));
        sequences.sort_by_key(|sequence| sequence[0].addr);
        Ok(LineTable {
            files,
            rows: sequences.into_iter().flatten().collect(),
        })
    }

    fn table_at(&self, pc: u32) -> Result<Option<Arc<LineTable>>> {
        match self.function_entry_at(pc) {
            Some(entry) => self.line_table(entry.unit).map(Some),
            None => Ok(None),
        }
    }

    pub fn line_at(&self, pc: u32) -> Result<Option<SourceLocation>> {
        let Some(table) = self.table_at(pc)? else {
            return Ok(None);
        };
        Ok(row_index(&table.rows, pc).and_then(|index| {
            let row = table.rows[index];
            (row.line != 0).then(|| SourceLocation {
                file: table
                    .files
                    .get(row.file as usize)
                    .cloned()
                    .unwrap_or_default(),
                line: row.line,
            })
        }))
    }

    /// Whether `pc` is exactly where a statement begins.
    pub fn is_statement_start(&self, pc: u32) -> Result<bool> {
        let Some(table) = self.table_at(pc)? else {
            return Ok(false);
        };
        let first = table.rows.partition_point(|row| row.addr < pc);
        Ok(table.rows[first..]
            .iter()
            .take_while(|row| row.addr == pc)
            .any(|row| row.is_stmt && !row.end_sequence && row.line != 0))
    }

    /// Statement addresses within `[low, high)`.
    pub fn statements_in(&self, low: u32, high: u32) -> Result<Vec<(u32, SourceLocation)>> {
        let Some(table) = self.table_at(low)? else {
            return Ok(Vec::new());
        };
        let first = table.rows.partition_point(|row| row.addr < low);
        Ok(table.rows[first..]
            .iter()
            .take_while(|row| row.addr < high)
            .filter(|row| row.is_stmt && !row.end_sequence && row.line != 0)
            .map(|row| {
                (
                    row.addr,
                    SourceLocation {
                        file: table
                            .files
                            .get(row.file as usize)
                            .cloned()
                            .unwrap_or_default(),
                        line: row.line,
                    },
                )
            })
            .collect())
    }

    /// Whether the code at `pc` was compiled with optimization on.
    pub fn is_optimized(&self, pc: u32) -> Result<bool> {
        let Some(entry) = self.function_entry_at(pc) else {
            return Ok(false);
        };
        let unit = self.unit(entry.unit)?;
        let unit_ref = unit.unit_ref(&self.dwarf);
        let mut tree = unit_ref.entries_tree(None)?;
        let root = tree.root()?;
        let Some(value) = root.entry().attr_value(gimli::DW_AT_producer) else {
            return Ok(false);
        };
        let producer = unit_ref.attr_string(value)?;
        let producer = String::from_utf8_lossy(producer.slice());
        Ok(producer
            .split(' ')
            .rfind(|flag| flag.starts_with("-O"))
            .is_some_and(|flag| flag != "-O0"))
    }

    /// Where to stop for "break at function". Unoptimized code stops at the
    /// second line entry, after the prologue has stored the arguments. In
    /// optimized code that entry may be on a branch not taken, while location
    /// lists are valid from the first instruction, so the entry itself is used.
    pub fn after_prologue(&self, low: u32, high: u32) -> Result<u32> {
        if self.is_optimized(low)? {
            return Ok(low);
        }
        let statements = self.statements_in(low, high)?;
        Ok(statements
            .iter()
            .find(|(addr, _)| *addr > low)
            .map_or(low, |(addr, _)| *addr))
    }

    /// Compiler-recorded paths best matching an editor's path. An exact match
    /// wins; otherwise the longest shared tail handles builds made elsewhere.
    pub(crate) fn matching_files(&self, path: &str) -> Vec<usize> {
        let path = paths::normalize(path);
        let path = path.as_ref();
        let files = &self.index.files;
        let exact: Vec<usize> = (0..files.len())
            .filter(|i| paths::same(self.index.str(files[*i].path), path))
            .collect();
        if !exact.is_empty() {
            return exact;
        }
        let best = (0..files.len())
            .map(|i| common_suffix(self.index.str(files[i].path), path))
            .max()
            .unwrap_or(0);
        if best == 0 {
            return Vec::new();
        }
        (0..files.len())
            .filter(|i| common_suffix(self.index.str(files[*i].path), path) == best)
            .collect()
    }

    /// Addresses for a source line, sliding forward at most ten lines when the
    /// requested one has no code. One address per contiguous block of the line.
    pub fn resolve_line(&self, path: &str, line: u32) -> Result<Option<ResolvedLine>> {
        let mut units = BTreeSet::new();
        let mut names = BTreeSet::new();
        for index in self.matching_files(path) {
            let entry = &self.index.files[index];
            names.insert(self.index.str(entry.path).to_owned());
            let start = entry.units_start as usize;
            units.extend(
                self.index.file_units[start..start + entry.units_len as usize]
                    .iter()
                    .copied(),
            );
        }

        let mut best_line = None::<u32>;
        let mut addresses = BTreeSet::new();
        let mut best_file = String::new();
        for unit in units {
            let table = self.line_table(unit)?;
            let is_wanted = |row: &Row| {
                table
                    .files
                    .get(row.file as usize)
                    .is_some_and(|file| names.contains(file))
            };
            let usable = |row: &Row| {
                row.is_stmt
                    && !row.end_sequence
                    && row.line >= line
                    && row.line <= line + MAX_LINE_SLIDE
                    && is_wanted(row)
            };
            let Some(found) = table
                .rows
                .iter()
                .filter(|r| usable(r))
                .map(|r| r.line)
                .min()
            else {
                continue;
            };
            if best_line.is_some_and(|best| found > best) {
                continue;
            }
            if best_line != Some(found) {
                addresses.clear();
                best_line = Some(found);
            }
            for (index, row) in table.rows.iter().enumerate() {
                if !usable(row) || row.line != found {
                    continue;
                }
                let continues_block = index > 0 && {
                    let previous = table.rows[index - 1];
                    !previous.end_sequence && previous.line == found && is_wanted(&previous)
                };
                if !continues_block {
                    addresses.insert(row.addr);
                    best_file = table.files[row.file as usize].clone();
                }
            }
        }
        Ok(best_line.map(|line| ResolvedLine {
            file: best_file,
            line,
            addresses: addresses.into_iter().collect(),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(addr: u32, line: u32, end_sequence: bool) -> Row {
        Row {
            addr,
            file: 0,
            line,
            is_stmt: true,
            end_sequence,
        }
    }

    #[test]
    fn row_lookup_respects_sequence_ends_and_duplicates() {
        let rows = [
            row(0x10, 1, false),
            row(0x14, 2, false),
            row(0x14, 3, false),
            row(0x20, 0, true),
            row(0x40, 9, false),
            row(0x44, 0, true),
        ];
        assert_eq!(row_index(&rows, 0x0F), None);
        assert_eq!(rows[row_index(&rows, 0x12).unwrap()].line, 1);
        assert_eq!(rows[row_index(&rows, 0x14).unwrap()].line, 3);
        assert_eq!(row_index(&rows, 0x30), None);
        assert_eq!(rows[row_index(&rows, 0x40).unwrap()].line, 9);
    }

    #[test]
    fn suffix_matching_counts_whole_components() {
        assert_eq!(common_suffix("/a/b/c.cpp", "/x/b/c.cpp"), 2);
        assert_eq!(common_suffix("/a/bc.cpp", "/a/c.cpp"), 0);
    }
}
