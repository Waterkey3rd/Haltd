//! Source and function breakpoints (DESIGN §5.5.2).
//!
//! One user breakpoint can need several addresses in optimized code. FPB
//! comparators are used first, keeping two free for stepping; the rest go to
//! flash breakpoints.

use std::collections::BTreeMap;

use probe_api::{BpHandle, BpKind, Probe, ProbeError};
use probe_jlink::actor::{Priority, ProbeHandle};
use symbols::{SourceLocation, SymbolDb};

use crate::project::FlashBreakpoints;

/// FPB comparators left free for temporary stepping breakpoints.
const RESERVED_FOR_STEPPING: usize = 2;

type Result<T> = std::result::Result<T, ProbeError>;

fn probe_call<R, F>(handle: &ProbeHandle, f: F) -> Result<R>
where
    R: Send + 'static,
    F: FnOnce(&mut dyn Probe) -> Result<R> + Send + 'static,
{
    handle.call_blocking(Priority::P0, f)?
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedBreakpoint {
    pub id: i64,
    pub verified: bool,
    pub file: Option<String>,
    pub line: u32,
    pub message: Option<String>,
    pub locations: usize,
    pub flash_locations: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Placement {
    pub id: i64,
    pub addr: u32,
    pub is_flash: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Request {
    Line { file: String, line: u32 },
    Function(String),
}

#[derive(Debug)]
struct Location {
    addr: u32,
    handle: BpHandle,
    is_flash: bool,
}

#[derive(Debug)]
struct Breakpoint {
    id: i64,
    request: Request,
    source: Option<SourceLocation>,
    locations: Vec<Location>,
    message: Option<String>,
}

pub struct BreakpointTable {
    hardware_limit: usize,
    policy: FlashBreakpoints,
    next_id: i64,
    entries: BTreeMap<i64, Breakpoint>,
}

impl BreakpointTable {
    pub fn new(hardware_total: usize, policy: FlashBreakpoints) -> Self {
        Self {
            hardware_limit: hardware_total.saturating_sub(RESERVED_FOR_STEPPING),
            policy,
            next_id: 1,
            entries: BTreeMap::new(),
        }
    }

    fn hardware_used(&self) -> usize {
        self.entries
            .values()
            .flat_map(|entry| &entry.locations)
            .filter(|location| !location.is_flash)
            .count()
    }

    fn choose_kind(&self, extra_hardware: usize) -> Option<BpKind> {
        let hardware_left = self.hardware_used() + extra_hardware < self.hardware_limit;
        match (self.policy, hardware_left) {
            (FlashBreakpoints::Always, _) => Some(BpKind::Flash),
            (FlashBreakpoints::Auto, true) | (FlashBreakpoints::Never, true) => Some(BpKind::Hw),
            (FlashBreakpoints::Auto, false) => Some(BpKind::Flash),
            (FlashBreakpoints::Never, false) => None,
        }
    }

    fn place(
        &self,
        handle: &ProbeHandle,
        addresses: &[u32],
    ) -> Result<(Vec<Location>, Option<String>)> {
        let mut locations = Vec::with_capacity(addresses.len());
        let mut hardware = 0;
        for address in addresses.iter().copied() {
            let Some(kind) = self.choose_kind(hardware) else {
                for placed in locations.drain(..) {
                    let Location { handle: bp, .. } = placed;
                    probe_call(handle, move |probe| probe.clear_bp(bp))?;
                }
                return Ok((
                    Vec::new(),
                    Some("no hardware breakpoint left and flash breakpoints are off".to_owned()),
                ));
            };
            let bp = probe_call(handle, move |probe| probe.set_bp(address, kind))?;
            hardware += usize::from(kind == BpKind::Hw);
            locations.push(Location {
                addr: address,
                handle: bp,
                is_flash: kind == BpKind::Flash,
            });
        }
        Ok((locations, None))
    }

    fn resolve(db: &SymbolDb, request: &Request) -> Option<(Option<SourceLocation>, Vec<u32>)> {
        match request {
            Request::Line { file, line } => {
                let resolved = db.resolve_line(file, *line).ok().flatten()?;
                Some((
                    Some(SourceLocation {
                        file: resolved.file,
                        line: resolved.line,
                    }),
                    resolved.addresses,
                ))
            }
            Request::Function(name) => {
                let functions = db.functions(name);
                let mut addresses = Vec::new();
                let mut source = None;
                for function in &functions {
                    let address = db.after_prologue(function.low, function.high).ok()?;
                    addresses.push(address);
                    source = source.or(db.line_at(address).ok().flatten());
                }
                (!addresses.is_empty()).then_some((source, addresses))
            }
        }
    }

    fn add(
        &mut self,
        db: &SymbolDb,
        handle: &ProbeHandle,
        request: Request,
    ) -> Result<ResolvedBreakpoint> {
        let id = self.next_id;
        self.next_id += 1;
        let mut breakpoint = Breakpoint {
            id,
            request,
            source: None,
            locations: Vec::new(),
            message: None,
        };
        self.bind(db, handle, &mut breakpoint)?;
        let resolved = Self::describe(&breakpoint);
        self.entries.insert(id, breakpoint);
        Ok(resolved)
    }

    fn bind(&self, db: &SymbolDb, handle: &ProbeHandle, breakpoint: &mut Breakpoint) -> Result<()> {
        match Self::resolve(db, &breakpoint.request) {
            Some((source, addresses)) => {
                let (locations, message) = self.place(handle, &addresses)?;
                breakpoint.source = source;
                breakpoint.locations = locations;
                breakpoint.message = message;
            }
            None => {
                breakpoint.source = None;
                breakpoint.locations = Vec::new();
                breakpoint.message = Some("no code at this location".to_owned());
            }
        }
        Ok(())
    }

    fn describe(breakpoint: &Breakpoint) -> ResolvedBreakpoint {
        let requested_line = match &breakpoint.request {
            Request::Line { line, .. } => *line,
            Request::Function(_) => 0,
        };
        let flash_locations = breakpoint.locations.iter().filter(|l| l.is_flash).count();
        let message = breakpoint.message.clone().or_else(|| {
            let count = breakpoint.locations.len();
            (count > 1 || flash_locations > 0)
                .then(|| format!("{count} location(s), {flash_locations} using flash breakpoints"))
        });
        ResolvedBreakpoint {
            id: breakpoint.id,
            verified: !breakpoint.locations.is_empty(),
            file: breakpoint.source.as_ref().map(|s| s.file.clone()),
            line: breakpoint
                .source
                .as_ref()
                .map_or(requested_line, |s| s.line),
            message,
            locations: breakpoint.locations.len(),
            flash_locations,
        }
    }

    fn remove(&mut self, handle: &ProbeHandle, id: i64) -> Result<()> {
        if let Some(entry) = self.entries.remove(&id) {
            for location in entry.locations {
                let bp = location.handle;
                probe_call(handle, move |probe| probe.clear_bp(bp))?;
            }
        }
        Ok(())
    }

    /// Replace all breakpoints in one file. Lines that stay keep their id and
    /// their placement, so re-sending the list does not rewrite flash.
    pub fn set_source(
        &mut self,
        db: &SymbolDb,
        handle: &ProbeHandle,
        file: &str,
        lines: &[u32],
    ) -> Result<Vec<ResolvedBreakpoint>> {
        let wanted = |request: &Request| {
            matches!(
                request,
                Request::Line { file: f, line } if f == file && lines.contains(line)
            )
        };
        let stale: Vec<i64> = self
            .entries
            .values()
            .filter(|entry| {
                matches!(&entry.request, Request::Line { file: f, .. } if f == file)
                    && !wanted(&entry.request)
            })
            .map(|entry| entry.id)
            .collect();
        for id in stale {
            self.remove(handle, id)?;
        }
        let mut results = Vec::with_capacity(lines.len());
        for line in lines {
            let request = Request::Line {
                file: file.to_owned(),
                line: *line,
            };
            let existing = self.entries.values().find(|entry| entry.request == request);
            results.push(match existing {
                Some(entry) => Self::describe(entry),
                None => self.add(db, handle, request)?,
            });
        }
        Ok(results)
    }

    pub fn set_functions(
        &mut self,
        db: &SymbolDb,
        handle: &ProbeHandle,
        names: &[String],
    ) -> Result<Vec<ResolvedBreakpoint>> {
        let stale: Vec<i64> = self
            .entries
            .values()
            .filter(
                |entry| matches!(&entry.request, Request::Function(name) if !names.contains(name)),
            )
            .map(|entry| entry.id)
            .collect();
        for id in stale {
            self.remove(handle, id)?;
        }
        let mut results = Vec::with_capacity(names.len());
        for name in names {
            let request = Request::Function(name.clone());
            let existing = self.entries.values().find(|entry| entry.request == request);
            results.push(match existing {
                Some(entry) => Self::describe(entry),
                None => self.add(db, handle, request)?,
            });
        }
        Ok(results)
    }

    /// Take every breakpoint off the target but remember the requests.
    pub fn unplace_all(&mut self, handle: &ProbeHandle) -> Result<()> {
        for entry in self.entries.values_mut() {
            for location in entry.locations.drain(..) {
                let bp = location.handle;
                probe_call(handle, move |probe| probe.clear_bp(bp))?;
            }
        }
        Ok(())
    }

    /// Remove every breakpoint from the target and forget it.
    pub fn clear(&mut self, handle: &ProbeHandle) -> Result<()> {
        self.unplace_all(handle)?;
        self.entries.clear();
        Ok(())
    }

    /// Resolve every request against a new image and place it again.
    pub fn rebind(
        &mut self,
        db: &SymbolDb,
        handle: &ProbeHandle,
    ) -> Result<Vec<ResolvedBreakpoint>> {
        let ids: Vec<i64> = self.entries.keys().copied().collect();
        let mut results = Vec::with_capacity(ids.len());
        for id in ids {
            let mut entry = self.entries.remove(&id).expect("id came from the map");
            self.bind(db, handle, &mut entry)?;
            results.push(Self::describe(&entry));
            self.entries.insert(id, entry);
        }
        Ok(results)
    }

    pub fn has_address(&self, addr: u32) -> bool {
        self.entries
            .values()
            .any(|entry| entry.locations.iter().any(|l| l.addr == addr))
    }

    pub fn ids_at(&self, addr: u32) -> Vec<i64> {
        self.entries
            .values()
            .filter(|entry| entry.locations.iter().any(|l| l.addr == addr))
            .map(|entry| entry.id)
            .collect()
    }

    pub fn location_at(&self, addr: u32) -> Option<SourceLocation> {
        self.entries
            .values()
            .find(|entry| entry.locations.iter().any(|l| l.addr == addr))
            .and_then(|entry| entry.source.clone())
    }

    pub fn placements(&self) -> Vec<Placement> {
        self.entries
            .values()
            .flat_map(|entry| {
                entry.locations.iter().map(|location| Placement {
                    id: entry.id,
                    addr: location.addr,
                    is_flash: location.is_flash,
                })
            })
            .collect()
    }
}
