//! Live Watch: expressions rendered as text on a background thread (§5.6.3).
//!
//! Only what the client has asked for is read. A collapsed struct is one entry
//! and its members exist only after `expand`.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, Weak};
use std::thread;
use std::time::{Duration, Instant};

use engine::session::HaltedMemory;
use probe_jlink::actor::Priority;
use serde::Serialize;
use symbols::{
    BaseEncoding, Evaluated, Expr, FormatOptions, MemoryReader, Number, Place, SymbolDb, TypeKind,
};
use tokio::sync::broadcast;

use crate::codec::{joined, Scalar};
use crate::{Bound, Sampler};

const TICK: Duration = Duration::from_millis(25);
const UPDATE_CHANNEL: usize = 64;
/// Largest object summarized on every refresh.
const SUMMARY_LIMIT: u64 = 4096;
const SUMMARY_OPTIONS: FormatOptions = FormatOptions {
    max_elements: 8,
    max_depth: 2,
};
/// Highest Live Watch refresh rate. Zero means refresh only when the target stops.
const MAX_RATE_HZ: u32 = 20;
const DEFAULT_RATE_HZ: u32 = 5;

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WatchValue {
    pub id: u32,
    pub value: String,
    pub raw_hex: Option<String>,
    pub address: Option<String>,
    pub error: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WatchInfo {
    pub id: u32,
    pub parent: Option<u32>,
    pub name: String,
    pub expr: String,
    #[serde(rename = "type")]
    pub type_name: String,
    pub expandable: bool,
    pub writable: bool,
    pub rate_hz: u32,
    pub value: String,
    pub error: Option<String>,
    /// Whether `expr` is a real expression that can go on the Timeline.
    pub plottable: bool,
    pub address: Option<String>,
    pub bit_range: Option<String>,
    pub size: Option<u32>,
    pub raw_hex: Option<String>,
    pub scalar_kind: Option<&'static str>,
}

struct Watch {
    parent: Option<u32>,
    name: String,
    expr: String,
    rate_hz: u32,
    place: Option<Place>,
    dynamic: bool,
    type_name: String,
    expandable: bool,
    writable: bool,
    error: Option<String>,
    value: String,
    address: Option<String>,
    bit_range: Option<String>,
    size: Option<u32>,
    raw_hex: Option<String>,
    scalar_kind: Option<&'static str>,
    next_due: Instant,
    /// Its members are shown, so reading the whole object would be wasted.
    expanded: bool,
}

impl Watch {
    fn info(&self, id: u32) -> WatchInfo {
        WatchInfo {
            id,
            parent: self.parent,
            name: self.name.clone(),
            expr: self.expr.clone(),
            type_name: self.type_name.clone(),
            expandable: self.expandable,
            writable: self.writable,
            rate_hz: self.rate_hz,
            value: self.value.clone(),
            error: self.error.clone(),
            plottable: self.place.is_some() && symbols::parse(&self.expr).is_ok(),
            address: self.address.clone(),
            bit_range: self.bit_range.clone(),
            size: self.size,
            raw_hex: self.raw_hex.clone(),
            scalar_kind: self.scalar_kind,
        }
    }

    fn clear_resolved(&mut self, error: String) {
        self.place = None;
        self.expandable = false;
        self.writable = false;
        self.value = String::new();
        self.address = None;
        self.bit_range = None;
        self.size = None;
        self.raw_hex = None;
        self.scalar_kind = None;
        self.error = Some(error);
    }

    fn update_payload(&self, id: u32) -> WatchValue {
        WatchValue {
            id,
            value: self.value.clone(),
            raw_hex: self.raw_hex.clone(),
            address: self.address.clone(),
            error: self.error.clone(),
        }
    }
}

#[derive(Default)]
struct Table {
    entries: BTreeMap<u32, Watch>,
    next_id: u32,
    refresh_all: bool,
}

pub(crate) struct Watches {
    table: Mutex<Table>,
    pub(crate) updates: broadcast::Sender<Arc<Vec<WatchValue>>>,
}

fn is_dynamic_expr(expr: &Expr) -> bool {
    match expr {
        Expr::Deref(_)
        | Expr::Arrow(_, _)
        | Expr::IndexBy(_, _)
        | Expr::Unary(_, _)
        | Expr::Binary(_, _, _)
        | Expr::Cast(_, _)
        | Expr::AddressOf(_) => true,
        Expr::Member(inner, _) | Expr::Index(inner, _) => is_dynamic_expr(inner),
        Expr::Path(_) | Expr::Integer(_) | Expr::Float(_) => false,
    }
}

fn little_endian(bytes: &[u8]) -> u64 {
    bytes
        .iter()
        .take(8)
        .rev()
        .fold(0, |value, byte| (value << 8) | u64::from(*byte))
}

fn format_hex_addr(addr: u32) -> String {
    format!("{addr:#010x}")
}

fn format_bit_range(place: &Place) -> Option<String> {
    place.bits.map(|bits| {
        if bits.bit_size <= 1 {
            format!("[{}]", bits.bit_offset)
        } else {
            format!(
                "[{}:{}]",
                bits.bit_offset + bits.bit_size - 1,
                bits.bit_offset
            )
        }
    })
}

fn classify_scalar(db: &SymbolDb, place: &Place) -> Option<(Scalar, &'static str)> {
    let scalar = Scalar::of(db, place)?;
    let (_, info) = db.strip(place.ty).ok()?;
    let kind = match &info.kind {
        TypeKind::Base(BaseEncoding::Bool) => "bool",
        TypeKind::Base(BaseEncoding::Float) => "float",
        TypeKind::Pointer(_) | TypeKind::Reference(_) => "pointer",
        TypeKind::Enum { .. } => "enum",
        TypeKind::Base(BaseEncoding::Signed | BaseEncoding::SignedChar) => "signed",
        _ => "unsigned",
    };
    Some((scalar, kind))
}

struct RenderedWatch {
    value: String,
    raw_hex: Option<String>,
    scalar_kind: Option<&'static str>,
    size: Option<u32>,
    error: Option<String>,
}

fn render_place_full(bound: &Bound, place: Place, memory: &mut dyn MemoryReader) -> RenderedWatch {
    let db = &bound.binding.db;
    let size = db.place_size(&place).ok().map(|s| s as u32);
    let mut raw_hex = None;
    let mut scalar_kind = None;

    if let Some((scalar, kind)) = classify_scalar(db, &place) {
        scalar_kind = Some(kind);
        let mut buf = [0u8; 8];
        let len = (scalar.size as usize).min(buf.len());
        if memory.read(place.addr, &mut buf[..len]).is_ok() {
            let raw = little_endian(&buf[..len]);
            let (bits_val, hex_digits) = match scalar.bits {
                Some(bits) => {
                    let shifted = raw >> bits.bit_offset;
                    let masked = if bits.bit_size < 64 {
                        shifted & ((1u64 << bits.bit_size) - 1)
                    } else {
                        shifted
                    };
                    (masked, (bits.bit_size as usize).div_ceil(4).max(1))
                }
                None => (raw, (scalar.size as usize * 2).max(1)),
            };
            raw_hex = Some(format!("0x{bits_val:0width$x}", width = hex_digits));
        }
    }

    let visualized = bound
        .rules
        .lock()
        .expect("rules lock")
        .as_ref()
        .and_then(|rules| rules.apply(db, memory, place))
        .and_then(|result| result.summary);
    if let Some(summary) = visualized {
        return RenderedWatch {
            value: summary,
            raw_hex,
            scalar_kind,
            size,
            error: None,
        };
    }
    if db
        .place_size(&place)
        .map_or(true, |len| len > SUMMARY_LIMIT)
    {
        return RenderedWatch {
            value: "{...}".to_owned(),
            raw_hex,
            scalar_kind,
            size,
            error: None,
        };
    }
    match db.render_place(&place, memory, &SUMMARY_OPTIONS) {
        Ok(value) => RenderedWatch {
            value,
            raw_hex,
            scalar_kind,
            size,
            error: None,
        },
        Err(error) => RenderedWatch {
            value: String::new(),
            raw_hex: None,
            scalar_kind,
            size,
            error: Some(error.to_string()),
        },
    }
}

fn children_of(bound: &Bound, place: Place, memory: &mut dyn MemoryReader) -> Vec<(String, Place)> {
    let db = &bound.binding.db;
    let shaped = bound
        .rules
        .lock()
        .expect("rules lock")
        .as_ref()
        .and_then(|rules| rules.apply(db, memory, place))
        .and_then(|result| result.children);
    match shaped {
        Some(children) => children,
        None => db.children(&place, memory).unwrap_or_default(),
    }
}

fn is_expandable(bound: &Bound, place: Place, memory: &mut dyn MemoryReader) -> bool {
    !children_of(bound, place, memory).is_empty()
}

fn resolve_evaluated(
    bound: &Bound,
    expression: &str,
    memory: &mut dyn MemoryReader,
) -> Result<Evaluated, String> {
    let parsed = symbols::parse(expression).map_err(|e| e.to_string())?;
    let rules = bound.rules.lock().expect("rules lock");
    let virtuals = rules
        .as_ref()
        .map(|rules| rules as &dyn symbols::VirtualMembers);
    bound
        .binding
        .db
        .evaluate_virtual(&parsed, memory, virtuals)
        .map_err(|e| e.to_string())
}

enum EvaluatedTick {
    Place {
        place: Place,
        rendered: RenderedWatch,
        expandable: bool,
        writable: bool,
        type_name: String,
    },
    Computed {
        type_name: String,
        value: String,
        raw_hex: Option<String>,
        scalar_kind: Option<&'static str>,
        size: Option<u32>,
    },
}

fn eval_for_watch(
    bound: &Bound,
    expression: &str,
    memory: &mut dyn MemoryReader,
    with_children_check: bool,
) -> Result<EvaluatedTick, String> {
    let db = &bound.binding.db;
    match resolve_evaluated(bound, expression, memory)? {
        Evaluated::Place(place) => {
            let rendered = render_place_full(bound, place, memory);
            let expandable = if with_children_check {
                is_expandable(bound, place, memory)
            } else {
                false
            };
            Ok(EvaluatedTick::Place {
                place,
                rendered,
                expandable,
                writable: Scalar::of(db, &place).is_some(),
                type_name: db.type_name(place.ty),
            })
        }
        Evaluated::Address { value, target } => Ok(EvaluatedTick::Computed {
            type_name: format!("{}*", db.type_name(target)),
            value: format_hex_addr(value),
            raw_hex: Some(format_hex_addr(value)),
            scalar_kind: Some("pointer"),
            size: Some(4),
        }),
        Evaluated::Value(Number::Int(n)) => {
            let fits_u32 = n >= i128::from(i32::MIN) && n <= i128::from(u32::MAX);
            let raw_hex = if fits_u32 {
                format!("{:#010x}", n as u32)
            } else {
                format!("{:#018x}", n as u64)
            };
            Ok(EvaluatedTick::Computed {
                type_name: "int".to_owned(),
                value: n.to_string(),
                raw_hex: Some(raw_hex),
                scalar_kind: Some(if n < 0 { "signed" } else { "unsigned" }),
                size: Some(if fits_u32 { 4 } else { 8 }),
            })
        }
        Evaluated::Value(Number::Float(f)) => Ok(EvaluatedTick::Computed {
            type_name: "double".to_owned(),
            value: f.to_string(),
            raw_hex: Some(format!("{:#018x}", f.to_bits())),
            scalar_kind: Some("float"),
            size: Some(8),
        }),
    }
}

fn remove_descendants(table: &mut Table, id: u32) {
    let children: Vec<u32> = table
        .entries
        .iter()
        .filter(|(_, watch)| watch.parent == Some(id))
        .map(|(child, _)| *child)
        .collect();
    for child in children {
        remove_descendants(table, child);
        table.entries.remove(&child);
    }
}

enum TickOutcome {
    StaticPlace(u32, RenderedWatch),
    DynamicExpr(
        u32,
        Result<EvaluatedTick, String>,
        Option<Vec<(String, Place)>>,
    ),
}

impl Watches {
    pub(crate) fn new() -> Self {
        Self {
            table: Mutex::new(Table {
                next_id: 1,
                ..Table::default()
            }),
            updates: broadcast::channel(UPDATE_CHANNEL).0,
        }
    }

    fn fill_place(
        &self,
        watch: &mut Watch,
        bound: &Bound,
        place: Place,
        memory: &mut dyn MemoryReader,
    ) {
        let db = &bound.binding.db;
        let rendered = render_place_full(bound, place, memory);
        watch.place = Some(place);
        watch.type_name = db.type_name(place.ty);
        watch.expandable = is_expandable(bound, place, memory);
        watch.writable = Scalar::of(db, &place).is_some();
        watch.address = Some(format_hex_addr(place.addr));
        watch.bit_range = format_bit_range(&place);
        watch.size = rendered.size;
        watch.raw_hex = rendered.raw_hex;
        watch.scalar_kind = rendered.scalar_kind;
        watch.value = rendered.value;
        watch.error = rendered.error;
    }

    fn apply_eval(&self, watch: &mut Watch, eval: Result<EvaluatedTick, String>) -> bool {
        let old = (
            watch.value.clone(),
            watch.raw_hex.clone(),
            watch.address.clone(),
            watch.error.clone(),
        );
        match eval {
            Ok(EvaluatedTick::Place {
                place,
                rendered,
                expandable,
                writable,
                type_name,
            }) => {
                watch.place = Some(place);
                watch.type_name = type_name;
                watch.expandable = expandable;
                watch.writable = writable;
                watch.address = Some(format_hex_addr(place.addr));
                watch.bit_range = format_bit_range(&place);
                watch.size = rendered.size;
                watch.raw_hex = rendered.raw_hex;
                watch.scalar_kind = rendered.scalar_kind;
                watch.value = rendered.value;
                watch.error = rendered.error;
            }
            Ok(EvaluatedTick::Computed {
                type_name,
                value,
                raw_hex,
                scalar_kind,
                size,
            }) => {
                watch.place = None;
                watch.type_name = type_name;
                watch.expandable = false;
                watch.writable = false;
                watch.address = None;
                watch.bit_range = None;
                watch.size = size;
                watch.raw_hex = raw_hex;
                watch.scalar_kind = scalar_kind;
                watch.value = value;
                watch.error = None;
            }
            Err(error) => {
                watch.clear_resolved(error);
            }
        }
        old != (
            watch.value.clone(),
            watch.raw_hex.clone(),
            watch.address.clone(),
            watch.error.clone(),
        )
    }

    /// Expressions are resolved again against a new target or image. Children
    /// are dropped: clients expand them afresh.
    pub(crate) fn rebind(&self, sampler: &Sampler) {
        let Some(bound) = sampler.bound() else { return };
        let mut memory = HaltedMemory::with_priority(bound.binding.handle.clone(), Priority::P3);
        let mut table = self.table.lock().expect("watch lock");
        table.entries.retain(|_, watch| watch.parent.is_none());
        for watch in table.entries.values_mut() {
            watch.expanded = false;
            watch.dynamic = symbols::parse(&watch.expr)
                .map(|parsed| is_dynamic_expr(&parsed))
                .unwrap_or(false);
            let eval = eval_for_watch(&bound, &watch.expr, &mut memory, true);
            self.apply_eval(watch, eval);
        }
    }

    pub(crate) fn refresh_on_stop(&self) {
        self.table.lock().expect("watch lock").refresh_all = true;
    }

    /// Render every watch that is due, publishing the ones that changed.
    fn tick(&self, sampler: &Sampler) {
        let Some(bound) = sampler.bound() else { return };
        let now = Instant::now();
        enum DueItem {
            Static(u32, Place),
            Dynamic {
                id: u32,
                expr: String,
                expanded: bool,
                prev_addr: Option<u32>,
            },
        }
        let due: Vec<DueItem> = {
            let mut table = self.table.lock().expect("watch lock");
            let refresh_all = std::mem::take(&mut table.refresh_all);
            table
                .entries
                .iter_mut()
                .filter_map(|(id, watch)| {
                    let is_due = refresh_all || (watch.rate_hz > 0 && watch.next_due <= now);
                    if !is_due {
                        return None;
                    }
                    if watch.dynamic {
                        if watch.rate_hz > 0 {
                            watch.next_due = now + Duration::from_secs(1) / watch.rate_hz;
                        }
                        return Some(DueItem::Dynamic {
                            id: *id,
                            expr: watch.expr.clone(),
                            expanded: watch.expanded,
                            prev_addr: watch.place.map(|p| p.addr),
                        });
                    }
                    let place = watch.place.filter(|_| !watch.expanded)?;
                    if watch.rate_hz > 0 {
                        watch.next_due = now + Duration::from_secs(1) / watch.rate_hz;
                    }
                    Some(DueItem::Static(*id, place))
                })
                .collect()
        };
        if due.is_empty() {
            return;
        }
        // The probe is used without holding the table, so clients stay responsive.
        let mut memory = HaltedMemory::with_priority(bound.binding.handle.clone(), Priority::P3);
        let outcomes: Vec<TickOutcome> = due
            .into_iter()
            .map(|item| match item {
                DueItem::Static(id, place) => {
                    TickOutcome::StaticPlace(id, render_place_full(&bound, place, &mut memory))
                }
                DueItem::Dynamic {
                    id,
                    expr,
                    expanded,
                    prev_addr,
                } => {
                    let eval = eval_for_watch(&bound, &expr, &mut memory, !expanded);
                    let new_children = if expanded {
                        match &eval {
                            Ok(EvaluatedTick::Place { place, .. })
                                if Some(place.addr) != prev_addr =>
                            {
                                Some(children_of(&bound, *place, &mut memory))
                            }
                            _ => None,
                        }
                    } else {
                        None
                    };
                    TickOutcome::DynamicExpr(id, eval, new_children)
                }
            })
            .collect();

        let mut changed = Vec::new();
        {
            let mut table = self.table.lock().expect("watch lock");
            for outcome in outcomes {
                match outcome {
                    TickOutcome::StaticPlace(id, rendered) => {
                        if let Some(watch) = table.entries.get_mut(&id) {
                            let is_changed = watch.value != rendered.value
                                || watch.raw_hex != rendered.raw_hex
                                || watch.error != rendered.error;
                            if is_changed {
                                watch.value = rendered.value;
                                watch.raw_hex = rendered.raw_hex;
                                watch.scalar_kind = rendered.scalar_kind;
                                watch.size = rendered.size;
                                watch.error = rendered.error;
                                changed.push(watch.update_payload(id));
                            }
                        }
                    }
                    TickOutcome::DynamicExpr(id, eval, new_children) => {
                        let Some(watch) = table.entries.get_mut(&id) else {
                            continue;
                        };
                        let was_expanded = watch.expanded;
                        if was_expanded {
                            let err = eval.as_ref().err().cloned();
                            let new_place = match &eval {
                                Ok(EvaluatedTick::Place { place, .. }) => Some(*place),
                                _ => None,
                            };
                            if let Some(err) = err {
                                let had_err = watch.error.as_deref() == Some(err.as_str());
                                watch.clear_resolved(err.clone());
                                if !had_err {
                                    changed.push(watch.update_payload(id));
                                }
                                let child_ids: Vec<u32> = table
                                    .entries
                                    .iter()
                                    .filter(|(_, w)| w.parent == Some(id))
                                    .map(|(cid, _)| *cid)
                                    .collect();
                                for cid in child_ids {
                                    if let Some(child) = table.entries.get_mut(&cid) {
                                        let child_changed =
                                            child.error.as_deref() != Some(err.as_str());
                                        child.clear_resolved(err.clone());
                                        if child_changed {
                                            changed.push(child.update_payload(cid));
                                        }
                                    }
                                }
                            } else if let Some(place) = new_place {
                                let new_addr_str = format_hex_addr(place.addr);
                                let parent_changed = watch.address.as_deref()
                                    != Some(new_addr_str.as_str())
                                    || watch.error.is_some();
                                watch.place = Some(place);
                                watch.address = Some(new_addr_str);
                                watch.error = None;
                                if parent_changed {
                                    changed.push(watch.update_payload(id));
                                }
                                if let Some(children) = new_children {
                                    let child_ids: Vec<(u32, String)> = table
                                        .entries
                                        .iter()
                                        .filter(|(_, w)| w.parent == Some(id))
                                        .map(|(cid, w)| (*cid, w.name.clone()))
                                        .collect();
                                    for (cid, cname) in child_ids {
                                        if let Some((_, cplace)) =
                                            children.iter().find(|(n, _)| n == &cname)
                                        {
                                            let caddr = format_hex_addr(cplace.addr);
                                            let rendered =
                                                render_place_full(&bound, *cplace, &mut memory);
                                            if let Some(child) = table.entries.get_mut(&cid) {
                                                let c_changed = child.value != rendered.value
                                                    || child.raw_hex != rendered.raw_hex
                                                    || child.address.as_deref()
                                                        != Some(caddr.as_str())
                                                    || child.error != rendered.error;
                                                child.place = Some(*cplace);
                                                child.address = Some(caddr);
                                                child.bit_range = format_bit_range(cplace);
                                                child.size = rendered.size;
                                                child.value = rendered.value;
                                                child.raw_hex = rendered.raw_hex;
                                                child.scalar_kind = rendered.scalar_kind;
                                                child.error = rendered.error;
                                                if c_changed {
                                                    changed.push(child.update_payload(cid));
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        } else if self.apply_eval(watch, eval) {
                            changed.push(watch.update_payload(id));
                        }
                    }
                }
            }
        }
        if !changed.is_empty() {
            let _ = self.updates.send(Arc::new(changed));
        }
    }
}

pub(crate) fn spawn_worker(sampler: &Arc<Sampler>) {
    let weak: Weak<Sampler> = Arc::downgrade(sampler);
    thread::Builder::new()
        .name("live-watch".to_owned())
        .spawn(move || {
            while let Some(sampler) = weak.upgrade() {
                sampler.watches.tick(&sampler);
                drop(sampler);
                thread::sleep(TICK);
            }
        })
        .expect("spawning the watch thread");
}

fn check_rate(rate: u32) -> Result<u32, String> {
    if rate <= MAX_RATE_HZ {
        Ok(rate)
    } else {
        Err(format!("the refresh rate must be 0 to {MAX_RATE_HZ} Hz"))
    }
}

/// The expression for a member, so that it can be plotted or watched by itself.
fn member_path(parent: &str, member: &str) -> String {
    if member == "*" {
        format!("(*{parent})")
    } else {
        joined(parent, member)
    }
}

impl Sampler {
    /// Add a watch. It is kept even if it does not resolve yet, so that a
    /// variable can come back after a rebuild.
    pub fn watch_add(&self, expression: &str, rate_hz: Option<u32>) -> Result<WatchInfo, String> {
        let bound = self.bound();
        let default_rate = bound
            .as_ref()
            .map_or(DEFAULT_RATE_HZ, |b| b.binding.config.watch_rate_hz);
        let rate_hz = check_rate(rate_hz.unwrap_or(default_rate))?;
        let parsed = symbols::parse(expression).map_err(|e| e.to_string())?;
        let dynamic = is_dynamic_expr(&parsed);
        let mut watch = Watch {
            parent: None,
            name: expression.to_owned(),
            expr: expression.to_owned(),
            rate_hz,
            place: None,
            dynamic,
            type_name: String::new(),
            expandable: false,
            writable: false,
            error: Some("no target is connected".to_owned()),
            value: String::new(),
            address: None,
            bit_range: None,
            size: None,
            raw_hex: None,
            scalar_kind: None,
            next_due: Instant::now(),
            expanded: false,
        };
        if let Some(bound) = &bound {
            let mut memory =
                HaltedMemory::with_priority(bound.binding.handle.clone(), Priority::P3);
            let eval = eval_for_watch(bound, expression, &mut memory, true);
            self.watches.apply_eval(&mut watch, eval);
        }
        let mut table = self.watches.table.lock().expect("watch lock");
        let id = table.next_id;
        table.next_id += 1;
        let info = watch.info(id);
        table.entries.insert(id, watch);
        Ok(info)
    }

    /// Replace the expression of a root watch in place, keeping its ID and refresh rate.
    pub fn watch_modify(&self, id: u32, expression: &str) -> Result<WatchInfo, String> {
        let parsed = symbols::parse(expression).map_err(|e| e.to_string())?;
        let dynamic = is_dynamic_expr(&parsed);
        let bound = self.bound();
        let mut table = self.watches.table.lock().expect("watch lock");
        let is_root = table
            .entries
            .get(&id)
            .ok_or_else(|| format!("no watch {id}"))?
            .parent
            .is_none();
        if !is_root {
            return Err("only root watches can be edited".to_owned());
        }
        remove_descendants(&mut table, id);
        let watch = table.entries.get_mut(&id).expect("checked above");
        watch.name = expression.to_owned();
        watch.expr = expression.to_owned();
        watch.dynamic = dynamic;
        watch.expanded = false;
        watch.next_due = Instant::now();
        if let Some(bound) = &bound {
            let mut memory =
                HaltedMemory::with_priority(bound.binding.handle.clone(), Priority::P3);
            let eval = eval_for_watch(bound, expression, &mut memory, true);
            self.watches.apply_eval(watch, eval);
        } else {
            watch.clear_resolved("no target is connected".to_owned());
        }
        Ok(watch.info(id))
    }

    pub fn watch_remove(&self, id: u32) -> Result<(), String> {
        let mut table = self.watches.table.lock().expect("watch lock");
        if table.entries.remove(&id).is_none() {
            return Err(format!("no watch {id}"));
        }
        remove_descendants(&mut table, id);
        Ok(())
    }

    /// Forget an entry's members and go back to summarizing the whole object.
    pub fn watch_collapse(&self, id: u32) -> Result<(), String> {
        let mut table = self.watches.table.lock().expect("watch lock");
        remove_descendants(&mut table, id);
        let watch = table
            .entries
            .get_mut(&id)
            .ok_or_else(|| format!("no watch {id}"))?;
        watch.expanded = false;
        watch.next_due = Instant::now();
        Ok(())
    }

    pub fn watch_list(&self) -> Vec<WatchInfo> {
        let table = self.watches.table.lock().expect("watch lock");
        table
            .entries
            .iter()
            .map(|(id, watch)| watch.info(*id))
            .collect()
    }

    /// Set the refresh rate of an entry and of whatever is open beneath it.
    pub fn watch_set_rate(&self, id: u32, rate_hz: u32) -> Result<(), String> {
        let rate_hz = check_rate(rate_hz)?;
        let mut table = self.watches.table.lock().expect("watch lock");
        if !table.entries.contains_key(&id) {
            return Err(format!("no watch {id}"));
        }
        let mut pending = vec![id];
        while let Some(current) = pending.pop() {
            pending.extend(
                table
                    .entries
                    .iter()
                    .filter(|(_, watch)| watch.parent == Some(current))
                    .map(|(child, _)| *child),
            );
            if let Some(watch) = table.entries.get_mut(&current) {
                watch.rate_hz = rate_hz;
                watch.next_due = Instant::now();
            }
        }
        Ok(())
    }

    /// Create watches for the members of an entry, replacing any existing ones.
    pub fn watch_expand(&self, id: u32) -> Result<Vec<WatchInfo>, String> {
        let bound = self.bound().ok_or("no target is connected")?;
        let (place, rate_hz, expr) = {
            let mut table = self.watches.table.lock().expect("watch lock");
            let watch = table
                .entries
                .get(&id)
                .ok_or_else(|| format!("no watch {id}"))?;
            let details = (watch.place, watch.rate_hz, watch.expr.clone());
            remove_descendants(&mut table, id);
            if let Some(watch) = table.entries.get_mut(&id) {
                watch.expanded = true;
            }
            details
        };
        let place = place.ok_or("this watch has not resolved")?;
        let mut memory = HaltedMemory::with_priority(bound.binding.handle.clone(), Priority::P3);
        let children = children_of(&bound, place, &mut memory);
        let mut created = Vec::with_capacity(children.len());
        for (name, child_place) in children {
            let child_expr = member_path(&expr, &name);
            let dynamic = symbols::parse(&child_expr)
                .map(|parsed| is_dynamic_expr(&parsed))
                .unwrap_or(false);
            let mut watch = Watch {
                parent: Some(id),
                expr: child_expr,
                name,
                rate_hz,
                place: None,
                dynamic,
                type_name: String::new(),
                expandable: false,
                writable: false,
                error: None,
                value: String::new(),
                address: None,
                bit_range: None,
                size: None,
                raw_hex: None,
                scalar_kind: None,
                next_due: Instant::now(),
                expanded: false,
            };
            self.watches
                .fill_place(&mut watch, &bound, child_place, &mut memory);
            let mut table = self.watches.table.lock().expect("watch lock");
            let child = table.next_id;
            table.next_id += 1;
            created.push(watch.info(child));
            table.entries.insert(child, watch);
        }
        Ok(created)
    }

    pub fn watch_write(&self, id: u32, value: &str) -> Result<(), String> {
        let bound = self.bound().ok_or("no target is connected")?;
        let place = self
            .watches
            .table
            .lock()
            .expect("watch lock")
            .entries
            .get(&id)
            .and_then(|watch| watch.place)
            .ok_or_else(|| format!("watch {id} has no location"))?;
        let scalar =
            Scalar::of(&bound.binding.db, &place).ok_or("only numeric values can be written")?;
        Self::write_scalar(&bound, place, scalar, value)?;
        // Show the new value without waiting for the next refresh.
        self.watches.table.lock().expect("watch lock").refresh_all = true;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rates_run_from_off_to_twenty_hertz() {
        assert_eq!(check_rate(0), Ok(0));
        assert_eq!(check_rate(5), Ok(5));
        assert_eq!(check_rate(20), Ok(20));
        assert!(check_rate(21).is_err());
    }

    #[test]
    fn disconnected_watch_add_uses_valid_default_rate() {
        let tsdb = Arc::new(tsdb::Tsdb::default());
        let sampler = Sampler::new(tsdb);
        let added = sampler.watch_add("g_counter", None).unwrap();
        assert_eq!(added.rate_hz, 5);
        assert!(added.error.is_some());
    }

    #[test]
    fn watch_modify_replaces_expression_in_place() {
        let tsdb = Arc::new(tsdb::Tsdb::default());
        let sampler = Sampler::new(tsdb);
        let first = sampler.watch_add("g_a", Some(2)).unwrap();
        let modified = sampler.watch_modify(first.id, "g_b + 1").unwrap();
        assert_eq!(modified.id, first.id);
        assert_eq!(modified.expr, "g_b + 1");
        assert_eq!(modified.rate_hz, 2);
    }

    #[test]
    fn dynamic_expression_detection() {
        assert!(!is_dynamic_expr(&symbols::parse("g_a.b[2]").unwrap()));
        assert!(is_dynamic_expr(&symbols::parse("*g_ptr").unwrap()));
        assert!(is_dynamic_expr(&symbols::parse("g_ptr->x").unwrap()));
        assert!(is_dynamic_expr(&symbols::parse("g_arr[g_idx]").unwrap()));
        assert!(is_dynamic_expr(&symbols::parse("g_adc * 3.3").unwrap()));
        assert!(is_dynamic_expr(&symbols::parse("&g_a").unwrap()));
    }

    #[test]
    fn member_paths_read_like_expressions() {
        assert_eq!(member_path("a", "b"), "a.b");
        assert_eq!(member_path("a.b", "[3]"), "a.b[3]");
        assert_eq!(member_path("a", "<Base>"), "a");
        assert_eq!(member_path("p", "*"), "(*p)");
        assert!(symbols::parse(&member_path("(*p)", "x")).is_ok());
    }
}
