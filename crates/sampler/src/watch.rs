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
use symbols::{Evaluated, FormatOptions, MemoryReader, Place, SymbolDb};
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
const MAX_RATE_HZ: u32 = 5;

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WatchValue {
    pub id: u32,
    pub value: String,
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
}

struct Watch {
    parent: Option<u32>,
    name: String,
    expr: String,
    rate_hz: u32,
    place: Option<Place>,
    type_name: String,
    expandable: bool,
    writable: bool,
    error: Option<String>,
    value: String,
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

fn render(bound: &Bound, place: Place, memory: &mut dyn MemoryReader) -> String {
    let db = &bound.binding.db;
    let visualized = bound
        .rules
        .lock()
        .expect("rules lock")
        .as_ref()
        .and_then(|rules| rules.apply(db, memory, place))
        .and_then(|result| result.summary);
    if let Some(summary) = visualized {
        return summary;
    }
    if db
        .place_size(&place)
        .map_or(true, |size| size > SUMMARY_LIMIT)
    {
        return "{...}".to_owned();
    }
    db.render_place(&place, memory, &SUMMARY_OPTIONS)
        .unwrap_or_else(|error| format!("<{error}>"))
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

fn resolve(
    db: &SymbolDb,
    expression: &str,
    memory: &mut dyn MemoryReader,
) -> Result<Place, String> {
    let parsed = symbols::parse(expression).map_err(|e| e.to_string())?;
    match db.evaluate(&parsed, memory).map_err(|e| e.to_string())? {
        Evaluated::Place(place) => Ok(place),
        Evaluated::Address { .. } | Evaluated::Value(_) => {
            Err("this expression has no storage to watch".to_owned())
        }
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

    fn fill(&self, watch: &mut Watch, bound: &Bound, place: Place, memory: &mut dyn MemoryReader) {
        let db = &bound.binding.db;
        watch.place = Some(place);
        watch.type_name = db.type_name(place.ty);
        watch.expandable = is_expandable(bound, place, memory);
        watch.writable = Scalar::of(db, &place).is_some();
        watch.value = render(bound, place, memory);
        watch.error = None;
    }

    /// Expressions are resolved again against a new target or image. Children
    /// are dropped: clients expand them afresh.
    pub(crate) fn rebind(&self, sampler: &Sampler) {
        let Some(bound) = sampler.bound() else { return };
        let mut memory = HaltedMemory::with_priority(bound.binding.handle.clone(), Priority::P3);
        let db = bound.binding.db.clone();
        let mut table = self.table.lock().expect("watch lock");
        table.entries.retain(|_, watch| watch.parent.is_none());
        for watch in table.entries.values_mut() {
            watch.expanded = false;
            match resolve(&db, &watch.expr, &mut memory) {
                Ok(place) => self.fill(watch, &bound, place, &mut memory),
                Err(error) => {
                    watch.place = None;
                    watch.expandable = false;
                    watch.value = String::new();
                    watch.error = Some(error);
                }
            }
        }
    }

    pub(crate) fn refresh_on_stop(&self) {
        self.table.lock().expect("watch lock").refresh_all = true;
    }

    /// Render every watch that is due, publishing the ones that changed.
    fn tick(&self, sampler: &Sampler) {
        let Some(bound) = sampler.bound() else { return };
        let now = Instant::now();
        let due: Vec<(u32, Place)> = {
            let mut table = self.table.lock().expect("watch lock");
            let refresh_all = std::mem::take(&mut table.refresh_all);
            table
                .entries
                .iter_mut()
                .filter_map(|(id, watch)| {
                    let place = watch.place.filter(|_| !watch.expanded)?;
                    let is_due = refresh_all || (watch.rate_hz > 0 && watch.next_due <= now);
                    if !is_due {
                        return None;
                    }
                    if watch.rate_hz > 0 {
                        watch.next_due = now + Duration::from_secs(1) / watch.rate_hz;
                    }
                    Some((*id, place))
                })
                .collect()
        };
        if due.is_empty() {
            return;
        }
        // The probe is used without holding the table, so clients stay responsive.
        let mut memory = HaltedMemory::with_priority(bound.binding.handle.clone(), Priority::P3);
        let rendered: Vec<(u32, String)> = due
            .into_iter()
            .map(|(id, place)| (id, render(&bound, place, &mut memory)))
            .collect();

        let mut changed = Vec::new();
        {
            let mut table = self.table.lock().expect("watch lock");
            for (id, value) in rendered {
                if let Some(watch) = table.entries.get_mut(&id) {
                    if watch.value != value {
                        watch.value.clone_from(&value);
                        changed.push(WatchValue { id, value });
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
            .map_or(10, |b| b.binding.config.watch_rate_hz);
        let rate_hz = check_rate(rate_hz.unwrap_or(default_rate))?;
        symbols::parse(expression).map_err(|e| e.to_string())?;
        let mut watch = Watch {
            parent: None,
            name: expression.to_owned(),
            expr: expression.to_owned(),
            rate_hz,
            place: None,
            type_name: String::new(),
            expandable: false,
            writable: false,
            error: Some("no target is connected".to_owned()),
            value: String::new(),
            next_due: Instant::now(),
            expanded: false,
        };
        if let Some(bound) = &bound {
            let mut memory =
                HaltedMemory::with_priority(bound.binding.handle.clone(), Priority::P3);
            match resolve(&bound.binding.db, expression, &mut memory) {
                Ok(place) => self.watches.fill(&mut watch, bound, place, &mut memory),
                Err(error) => watch.error = Some(error),
            }
        }
        let mut table = self.watches.table.lock().expect("watch lock");
        let id = table.next_id;
        table.next_id += 1;
        let info = watch.info(id);
        table.entries.insert(id, watch);
        Ok(info)
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
            let mut watch = Watch {
                parent: Some(id),
                expr: member_path(&expr, &name),
                name,
                rate_hz,
                place: None,
                type_name: String::new(),
                expandable: false,
                writable: false,
                error: None,
                value: String::new(),
                next_due: Instant::now(),
                expanded: false,
            };
            self.watches
                .fill(&mut watch, &bound, child_place, &mut memory);
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
    fn rates_run_from_off_to_five_hertz() {
        assert_eq!(check_rate(0), Ok(0));
        assert_eq!(check_rate(5), Ok(5));
        assert!(check_rate(6).is_err());
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
