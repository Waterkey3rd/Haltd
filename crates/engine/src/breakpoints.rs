//! Source and function breakpoints (DESIGN §5.5.2).
//!
//! One user breakpoint can need several addresses in optimized code. FPB
//! comparators are used first, keeping two free for stepping; the rest go to
//! flash breakpoints.

use std::collections::BTreeMap;

use probe_api::{BpHandle, BpKind, Probe, ProbeError, WatchpointSpec, WpAccess, WpHandle};
use probe_jlink::actor::{Priority, ProbeHandle};
use symbols::{SourceLocation, SymbolDb};

use crate::conditions::{Behavior, BreakpointOptions};
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

/// One breakpoint of a source file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LineBreakpoint {
    pub line: u32,
    pub options: BreakpointOptions,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FunctionBreakpoint {
    pub name: String,
    pub options: BreakpointOptions,
}

/// A watched range of memory. DWT needs a power-of-two size, aligned to itself.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DataRequest {
    pub addr: u32,
    pub size: u32,
    pub access: WpAccess,
    /// Lets the breakpoint follow its variable to a new address after reload.
    pub expression: Option<String>,
    pub options: BreakpointOptions,
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
    behavior: Behavior,
}

#[derive(Debug)]
struct DataBreakpoint {
    id: i64,
    request: DataRequest,
    handle: Option<WpHandle>,
    message: Option<String>,
    behavior: Behavior,
}

pub struct BreakpointTable {
    hardware_limit: usize,
    policy: FlashBreakpoints,
    next_id: i64,
    entries: BTreeMap<i64, Breakpoint>,
    data: BTreeMap<i64, DataBreakpoint>,
}

impl BreakpointTable {
    pub fn new(hardware_total: usize, policy: FlashBreakpoints) -> Self {
        Self {
            hardware_limit: hardware_total.saturating_sub(RESERVED_FOR_STEPPING),
            policy,
            next_id: 1,
            entries: BTreeMap::new(),
            data: BTreeMap::new(),
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
        options: &BreakpointOptions,
    ) -> Result<ResolvedBreakpoint> {
        let id = self.next_id;
        self.next_id += 1;
        let mut breakpoint = Breakpoint {
            id,
            request,
            source: None,
            locations: Vec::new(),
            message: None,
            behavior: Behavior::new(options.clone()),
        };
        self.bind(db, handle, &mut breakpoint)?;
        let resolved = Self::describe(&breakpoint);
        self.entries.insert(id, breakpoint);
        Ok(resolved)
    }

    fn bind(&self, db: &SymbolDb, handle: &ProbeHandle, breakpoint: &mut Breakpoint) -> Result<()> {
        if let Some(error) = &breakpoint.behavior.error {
            breakpoint.message = Some(error.clone());
            return Ok(());
        }
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

    /// Apply new options to a breakpoint that stays. Its placement is kept
    /// when possible, since replacing it could mean rewriting flash.
    fn update_options(
        &mut self,
        db: &SymbolDb,
        handle: &ProbeHandle,
        id: i64,
        options: &BreakpointOptions,
    ) -> Result<ResolvedBreakpoint> {
        let mut entry = self.entries.remove(&id).expect("id came from the map");
        if entry.behavior.options != *options {
            entry.behavior = Behavior::new(options.clone());
            if entry.behavior.error.is_some() {
                for location in entry.locations.drain(..) {
                    let bp = location.handle;
                    probe_call(handle, move |probe| probe.clear_bp(bp))?;
                }
            }
            if entry.locations.is_empty() {
                self.bind(db, handle, &mut entry)?;
            } else {
                entry.message = None;
            }
        }
        let resolved = Self::describe(&entry);
        self.entries.insert(id, entry);
        Ok(resolved)
    }

    /// Replace all breakpoints in one file. Lines that stay keep their id and
    /// their placement, so re-sending the list does not rewrite flash.
    pub fn set_source(
        &mut self,
        db: &SymbolDb,
        handle: &ProbeHandle,
        file: &str,
        lines: &[LineBreakpoint],
    ) -> Result<Vec<ResolvedBreakpoint>> {
        let wanted = |request: &Request| {
            matches!(
                request,
                Request::Line { file: f, line } if f == file && lines.iter().any(|b| b.line == *line)
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
        for breakpoint in lines {
            let request = Request::Line {
                file: file.to_owned(),
                line: breakpoint.line,
            };
            results.push(self.add_or_update(db, handle, request, &breakpoint.options)?);
        }
        Ok(results)
    }

    pub fn set_functions(
        &mut self,
        db: &SymbolDb,
        handle: &ProbeHandle,
        functions: &[FunctionBreakpoint],
    ) -> Result<Vec<ResolvedBreakpoint>> {
        let names: Vec<&String> = functions.iter().map(|function| &function.name).collect();
        let stale: Vec<i64> = self
            .entries
            .values()
            .filter(
                |entry| matches!(&entry.request, Request::Function(name) if !names.contains(&name)),
            )
            .map(|entry| entry.id)
            .collect();
        for id in stale {
            self.remove(handle, id)?;
        }
        let mut results = Vec::with_capacity(functions.len());
        for function in functions {
            let request = Request::Function(function.name.clone());
            results.push(self.add_or_update(db, handle, request, &function.options)?);
        }
        Ok(results)
    }

    fn add_or_update(
        &mut self,
        db: &SymbolDb,
        handle: &ProbeHandle,
        request: Request,
        options: &BreakpointOptions,
    ) -> Result<ResolvedBreakpoint> {
        let existing = self
            .entries
            .values()
            .find(|entry| entry.request == request)
            .map(|entry| entry.id);
        match existing {
            Some(id) => self.update_options(db, handle, id, options),
            None => self.add(db, handle, request, options),
        }
    }

    fn describe_data(entry: &DataBreakpoint) -> ResolvedBreakpoint {
        ResolvedBreakpoint {
            id: entry.id,
            verified: entry.handle.is_some(),
            file: None,
            line: 0,
            message: entry.message.clone(),
            locations: usize::from(entry.handle.is_some()),
            flash_locations: 0,
        }
    }

    fn place_data(handle: &ProbeHandle, entry: &mut DataBreakpoint) -> Result<()> {
        entry.message = None;
        if let Some(error) = &entry.behavior.error {
            entry.message = Some(error.clone());
            return Ok(());
        }
        let DataRequest {
            addr, size, access, ..
        } = entry.request;
        if !size.is_power_of_two() || !addr.is_multiple_of(size) {
            entry.message = Some(format!(
                "the hardware can only watch a power-of-two size aligned to itself, not {size} bytes at {addr:#010x}"
            ));
            return Ok(());
        }
        let spec = WatchpointSpec {
            addr,
            addr_mask: size - 1,
            access,
        };
        match handle.call_blocking(Priority::P0, move |probe: &mut dyn Probe| {
            probe.set_wp(&spec)
        })? {
            Ok(wp) => entry.handle = Some(wp),
            Err(error) => entry.message = Some(format!("cannot watch this address: {error}")),
        }
        Ok(())
    }

    fn unplace_data(handle: &ProbeHandle, entry: &mut DataBreakpoint) -> Result<()> {
        if let Some(wp) = entry.handle.take() {
            probe_call(handle, move |probe| probe.clear_wp(wp))?;
        }
        Ok(())
    }

    /// Replace the data breakpoints. Unchanged ones keep their comparator.
    pub fn set_data(
        &mut self,
        handle: &ProbeHandle,
        requests: &[DataRequest],
    ) -> Result<Vec<ResolvedBreakpoint>> {
        let same_target = |a: &DataRequest, b: &DataRequest| {
            a.addr == b.addr && a.size == b.size && a.access == b.access
        };
        let stale: Vec<i64> = self
            .data
            .values()
            .filter(|entry| !requests.iter().any(|r| same_target(r, &entry.request)))
            .map(|entry| entry.id)
            .collect();
        // Comparators must be free before the new ones are placed.
        for id in stale {
            if let Some(mut entry) = self.data.remove(&id) {
                Self::unplace_data(handle, &mut entry)?;
            }
        }
        let mut results = Vec::with_capacity(requests.len());
        for request in requests {
            let existing = self
                .data
                .values_mut()
                .find(|entry| same_target(request, &entry.request));
            if let Some(entry) = existing {
                if entry.behavior.options != request.options {
                    entry.behavior = Behavior::new(request.options.clone());
                }
                entry.request = request.clone();
                if entry.behavior.error.is_some() {
                    Self::unplace_data(handle, entry)?;
                    entry.message = entry.behavior.error.clone();
                } else if entry.handle.is_none() {
                    Self::place_data(handle, entry)?;
                }
                results.push(Self::describe_data(entry));
                continue;
            }
            let id = self.next_id;
            self.next_id += 1;
            let mut entry = DataBreakpoint {
                id,
                request: request.clone(),
                handle: None,
                message: None,
                behavior: Behavior::new(request.options.clone()),
            };
            Self::place_data(handle, &mut entry)?;
            results.push(Self::describe_data(&entry));
            self.data.insert(id, entry);
        }
        Ok(results)
    }

    /// Place data breakpoints again after a reload, re-resolving their
    /// variables through `resolve` (address and size).
    pub fn rebind_data(
        &mut self,
        handle: &ProbeHandle,
        mut resolve: impl FnMut(&str) -> Option<(u32, u32)>,
    ) -> Result<Vec<ResolvedBreakpoint>> {
        let mut results = Vec::with_capacity(self.data.len());
        for entry in self.data.values_mut() {
            let location = entry.request.expression.clone().map(|text| {
                let found = resolve(&text);
                (text, found)
            });
            match location {
                Some((_, Some((addr, size)))) => {
                    entry.request.addr = addr;
                    entry.request.size = size;
                    Self::place_data(handle, entry)?;
                }
                Some((text, None)) => {
                    entry.message = Some(format!("{text} was not found in the new image"));
                }
                None => {
                    entry.message =
                        Some("this variable may have moved; set the breakpoint again".to_owned());
                }
            }
            results.push(Self::describe_data(entry));
        }
        Ok(results)
    }

    /// Data breakpoints whose range covers a matched comparator address. With
    /// no usable match, every one of them is a candidate.
    pub fn data_ids_for(&self, matched: &[u32]) -> Vec<i64> {
        let hits: Vec<i64> = self
            .data
            .values()
            .filter(|entry| {
                let mask = !(entry.request.size.max(1) - 1);
                matched
                    .iter()
                    .any(|addr| addr & mask == entry.request.addr & mask)
            })
            .map(|entry| entry.id)
            .collect();
        if hits.is_empty() {
            self.data
                .values()
                .filter(|entry| entry.handle.is_some())
                .map(|entry| entry.id)
                .collect()
        } else {
            hits
        }
    }

    pub(crate) fn behavior(&self, id: i64) -> Option<&Behavior> {
        self.entries
            .get(&id)
            .map(|entry| &entry.behavior)
            .or_else(|| self.data.get(&id).map(|entry| &entry.behavior))
    }

    /// Count a hit and return the new total.
    pub(crate) fn count_hit(&mut self, id: i64) -> u64 {
        let behavior = match self.entries.get_mut(&id) {
            Some(entry) => Some(&mut entry.behavior),
            None => self.data.get_mut(&id).map(|entry| &mut entry.behavior),
        };
        behavior.map_or(0, |behavior| {
            behavior.hits += 1;
            behavior.hits
        })
    }

    pub fn reset_hits(&mut self) {
        for entry in self.entries.values_mut() {
            entry.behavior.hits = 0;
        }
        for entry in self.data.values_mut() {
            entry.behavior.hits = 0;
        }
    }

    /// Take every breakpoint off the target but remember the requests.
    pub fn unplace_all(&mut self, handle: &ProbeHandle) -> Result<()> {
        for entry in self.entries.values_mut() {
            for location in entry.locations.drain(..) {
                let bp = location.handle;
                probe_call(handle, move |probe| probe.clear_bp(bp))?;
            }
        }
        for entry in self.data.values_mut() {
            Self::unplace_data(handle, entry)?;
        }
        Ok(())
    }

    /// Remove every breakpoint from the target and forget it.
    pub fn clear(&mut self, handle: &ProbeHandle) -> Result<()> {
        self.unplace_all(handle)?;
        self.entries.clear();
        self.data.clear();
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
            entry.behavior.hits = 0;
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
