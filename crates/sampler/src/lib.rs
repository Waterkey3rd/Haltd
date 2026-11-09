//! Live Watch and Timeline sampling (DESIGN §5.6).
//!
//! A [`Sampler`] outlives any one probe connection. Watches and series are
//! remembered by expression and resolved again whenever a target is bound.

pub mod codec;
pub mod planner;
pub mod timesync;
mod watch;

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use engine::session::HaltedMemory;
use probe_api::{AccessWidth, HssCaps, Probe, ProbeError, ProbeInfo, HSS_FLAG_TIMESTAMP_US};
use probe_jlink::actor::{PeriodicId, Priority, ProbeHandle, QueueStats};
use serde::Serialize;
use serde_json::json;
use symbols::{Place, SymbolDb};
use tokio::sync::broadcast;
use tsdb::{EventKind, Series, Tsdb};
use visualizer::Visualizers;

use codec::{resolve_leaves, Leaf, Scalar};
use planner::{hss_budget, plan_hss, plan_poll, PollRange, Signal};
use timesync::ProbeClock;

pub use watch::{WatchInfo, WatchValue};

/// Records buffered per HSS read: a 10 ms read at 1 kHz needs far less.
const HSS_BUFFER: usize = 64 * 1024;
const HSS_READ_INTERVAL: Duration = Duration::from_millis(10);
/// Slower than this, an HSS gap would look like the target having halted.
const MIN_RATE_HZ: u32 = 20;
/// Polling is the overflow path for what HSS cannot hold, and shares the probe.
const MAX_POLL_HZ: u32 = 100;
/// A record reaches the host this long after it was taken, however promptly it
/// is read. Measured on a J-Link V9 against polled reads with host timestamps.
const HSS_DELIVERY_LATENCY_NS: i64 = 3_000_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    Auto,
    Hss,
    Poll,
}

impl Mode {
    pub fn parse(text: Option<&str>) -> Result<Self, String> {
        match text.unwrap_or("auto") {
            "auto" => Ok(Mode::Auto),
            "hss" => Ok(Mode::Hss),
            "poll" => Ok(Mode::Poll),
            other => Err(format!("unknown sampling mode {other:?}")),
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct SamplingConfig {
    pub hss_period_us: u32,
    pub watch_rate_hz: u32,
}

impl Default for SamplingConfig {
    fn default() -> Self {
        Self {
            hss_period_us: 1000,
            watch_rate_hz: 10,
        }
    }
}

/// Everything needed to sample a connected target.
#[derive(Clone)]
pub struct Binding {
    pub handle: ProbeHandle,
    pub db: Arc<SymbolDb>,
    pub device: String,
    pub elf: PathBuf,
    pub speed_khz: u32,
    pub probe: ProbeInfo,
    /// `(start, end, is_ram)`. Empty means the memory map is unknown.
    pub memory: Vec<(u32, u32, bool)>,
    pub visualizer_files: Vec<PathBuf>,
    pub config: SamplingConfig,
    pub rtt_config: Option<PathBuf>,
    pub workspace: Option<PathBuf>,
}

/// Told when a target becomes available or goes away.
pub trait BindListener: Send + Sync {
    fn bound(self: Arc<Self>, binding: &Binding);
    fn unbound(self: Arc<Self>);
}

impl Binding {
    fn region_of(&self, addr: u32, size: u32) -> Option<bool> {
        self.memory
            .iter()
            .find(|(start, end, _)| addr >= *start && addr + size <= *end)
            .map(|(_, _, is_ram)| *is_ram)
    }
}

pub(crate) struct Bound {
    pub binding: Binding,
    pub rules: Mutex<Option<Visualizers>>,
}

/// Actual state of one sampled value.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SeriesStatus {
    pub series_id: u32,
    pub name: String,
    pub root: String,
    pub actual_mode: &'static str,
    pub rate_hz: f64,
    pub error: Option<String>,
}

struct SeriesDef {
    series: Arc<Series>,
    root: String,
    leaf_name: String,
    mode: Mode,
    priority: i32,
    order: u64,
    /// None while its expression does not resolve.
    resolved: Option<Leaf>,
    actual: &'static str,
    error: Option<String>,
}

#[derive(Default)]
struct Counters {
    hss_records: AtomicU64,
    hss_lost: AtomicU64,
    hss_bytes: AtomicU64,
    hss_errors: AtomicU64,
    hss_halts: AtomicU64,
    poll_errors: AtomicU64,
    poll_cycles: AtomicU64,
}

#[derive(Default)]
struct State {
    bound: Option<Arc<Bound>>,
    defs: Vec<SeriesDef>,
    next_order: u64,
    hss_task: Option<PeriodicId>,
    poll_task: Option<PeriodicId>,
    hss_running: bool,
    hss_caps: Option<HssCaps>,
    planned_rates: (f64, f64),
    /// Chosen in the Timeline, in Hz, with zero meaning off. Outlives any one
    /// target connection.
    chosen_rate: Option<u32>,
}

impl State {
    fn is_off(&self) -> bool {
        self.chosen_rate == Some(0)
    }

    fn effective(&self, mut config: SamplingConfig) -> SamplingConfig {
        if let Some(hz) = self.chosen_rate.filter(|hz| *hz > 0) {
            config.hss_period_us = 1_000_000 / hz;
        }
        config
    }
}

/// The sampling rate in force, zero when sampling is off.
#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Rate {
    pub rate_hz: u32,
    pub max_hz: u32,
}

pub struct Sampler {
    tsdb: Arc<Tsdb>,
    state: Mutex<State>,
    counters: Arc<Counters>,
    pub(crate) watches: watch::Watches,
    listeners: Mutex<Vec<Arc<dyn BindListener>>>,
}

fn load_rules(files: &[PathBuf]) -> Option<Visualizers> {
    let rules = Visualizers::with_builtin().ok()?;
    for file in files {
        if let Err(error) = rules.load_file(file) {
            tracing::warn!("cannot load {}: {error}", file.display());
        }
    }
    Some(rules)
}

struct HssTarget {
    series: Arc<Series>,
    offset: usize,
    scalar: Scalar,
}

/// Runs on the probe thread every 10 ms.
struct HssReader {
    tsdb: Arc<Tsdb>,
    counters: Arc<Counters>,
    clock: ProbeClock,
    record_size: usize,
    targets: Vec<HssTarget>,
    buffer: Vec<u8>,
    filled: usize,
}

impl HssReader {
    fn tick(&mut self, probe: &mut dyn Probe) {
        let Some(hss) = probe.hss() else { return };
        let read = hss.hss_read(&mut self.buffer[self.filled..]);
        let now = self.tsdb.now_ns();
        let count = match read {
            Ok(count) => count,
            Err(error) => {
                if self.counters.hss_errors.fetch_add(1, Ordering::Relaxed) == 0 {
                    tracing::warn!("HSS read failed: {error}");
                }
                return;
            }
        };
        self.filled += count;
        self.counters
            .hss_bytes
            .fetch_add(count as u64, Ordering::Relaxed);
        let whole = self.filled - self.filled % self.record_size;
        let raw: Vec<u32> = self.buffer[..whole]
            .chunks_exact(self.record_size)
            .map(|record| u32::from_le_bytes([record[0], record[1], record[2], record[3]]))
            .collect();
        let before = (self.clock.lost, self.clock.discontinuities);
        let times = self.clock.convert(&raw, now - HSS_DELIVERY_LATENCY_NS);
        self.counters
            .hss_lost
            .fetch_add(self.clock.lost - before.0, Ordering::Relaxed);
        self.counters
            .hss_halts
            .fetch_add(self.clock.discontinuities - before.1, Ordering::Relaxed);
        self.counters
            .hss_records
            .fetch_add(raw.len() as u64, Ordering::Relaxed);

        for target in &self.targets {
            target.series.push_all(
                self.buffer[..whole]
                    .chunks_exact(self.record_size)
                    .zip(&times)
                    .map(|(record, t)| (*t, target.scalar.decode(&record[target.offset..]))),
            );
        }
        self.buffer.copy_within(whole..self.filled, 0);
        self.filled -= whole;
    }
}

struct PollTarget {
    series: Arc<Series>,
    offset: usize,
    scalar: Scalar,
}

/// One merged read and who wants it.
struct PollGroup {
    addr: u32,
    bytes: Vec<u8>,
    targets: Vec<PollTarget>,
}

struct PollReader {
    tsdb: Arc<Tsdb>,
    counters: Arc<Counters>,
    groups: Vec<PollGroup>,
}

impl PollReader {
    fn tick(&mut self, probe: &mut dyn Probe) {
        for group in &mut self.groups {
            let started = self.tsdb.now_ns();
            let read = probe.read_mem(group.addr, &mut group.bytes, AccessWidth::Auto);
            let t = started + (self.tsdb.now_ns() - started) / 2;
            if read.is_err() {
                self.counters.poll_errors.fetch_add(1, Ordering::Relaxed);
                continue;
            }
            for target in &group.targets {
                target
                    .series
                    .push(t, target.scalar.decode(&group.bytes[target.offset..]));
            }
        }
        self.counters.poll_cycles.fetch_add(1, Ordering::Relaxed);
    }
}

impl Sampler {
    pub fn new(tsdb: Arc<Tsdb>) -> Arc<Self> {
        let sampler = Arc::new(Self {
            tsdb,
            state: Mutex::new(State::default()),
            counters: Arc::new(Counters::default()),
            watches: watch::Watches::new(),
            listeners: Mutex::new(Vec::new()),
        });
        watch::spawn_worker(&sampler);
        sampler
    }

    pub fn add_listener(&self, listener: Arc<dyn BindListener>) {
        self.listeners.lock().expect("listener lock").push(listener);
    }

    fn listeners(&self) -> Vec<Arc<dyn BindListener>> {
        self.listeners.lock().expect("listener lock").clone()
    }

    pub fn tsdb(&self) -> &Arc<Tsdb> {
        &self.tsdb
    }

    pub fn subscribe_watches(&self) -> broadcast::Receiver<Arc<Vec<WatchValue>>> {
        self.watches.updates.subscribe()
    }

    pub(crate) fn bound(&self) -> Option<Arc<Bound>> {
        self.state.lock().expect("sampler lock").bound.clone()
    }

    /// Attach to a target and resolve every remembered expression against it.
    pub fn bind(self: &Arc<Self>, binding: Binding) {
        let binding_copy = binding.clone();
        let bound = Arc::new(Bound {
            rules: Mutex::new(load_rules(&binding.visualizer_files)),
            binding,
        });
        let mut state = self.state.lock().expect("sampler lock");
        self.stop_tasks(&mut state);
        let handle = bound.binding.handle.clone();
        state.hss_caps = handle
            .call_blocking(Priority::P0, |probe: &mut dyn Probe| {
                probe.hss().map(|hss| hss.hss_caps()).transpose()
            })
            .ok()
            .and_then(Result::ok)
            .flatten();
        state.bound = Some(bound.clone());
        let mut memory = HaltedMemory::with_priority(handle, Priority::P3);
        let rules = bound.rules.lock().expect("rules lock");
        let db = bound.binding.db.clone();

        // Series sharing a root are resolved together, in a single walk.
        let roots: Vec<String> = {
            let mut roots: Vec<String> = state.defs.iter().map(|d| d.root.clone()).collect();
            roots.sort();
            roots.dedup();
            roots
        };
        for root in roots {
            let leaves = resolve_leaves(&db, rules.as_ref(), &root, &mut memory);
            for def in state.defs.iter_mut().filter(|d| d.root == root) {
                match &leaves {
                    Ok(leaves) => match leaves.iter().find(|leaf| leaf.name == def.leaf_name) {
                        Some(leaf) => {
                            def.resolved = Some(leaf.clone());
                            def.error = None;
                        }
                        None => {
                            def.resolved = None;
                            def.error = Some(format!("{} no longer exists", def.leaf_name));
                        }
                    },
                    Err(error) => {
                        def.resolved = None;
                        def.error = Some(error.clone());
                    }
                }
            }
        }
        drop(rules);
        self.replan(&mut state);
        drop(state);
        self.watches.rebind(self);
        for listener in self.listeners() {
            listener.bound(&binding_copy);
        }
    }

    /// Stop sampling before the target goes away or is reprogrammed.
    pub fn unbind(&self) {
        for listener in self.listeners() {
            listener.unbound();
        }
        let mut state = self.state.lock().expect("sampler lock");
        self.stop_tasks(&mut state);
        state.bound = None;
    }

    fn stop_tasks(&self, state: &mut State) {
        let Some(bound) = state.bound.clone() else {
            state.hss_task = None;
            state.poll_task = None;
            state.hss_running = false;
            return;
        };
        let handle = &bound.binding.handle;
        if let Some(task) = state.hss_task.take() {
            let _ = handle.cancel(task);
        }
        if let Some(task) = state.poll_task.take() {
            let _ = handle.cancel(task);
        }
        if std::mem::take(&mut state.hss_running) {
            let stopped =
                handle.call_blocking(Priority::P0, |probe: &mut dyn Probe| match probe.hss() {
                    Some(hss) => hss.hss_stop(),
                    None => Ok::<(), ProbeError>(()),
                });
            if !matches!(stopped, Ok(Ok(()))) {
                tracing::warn!("stopping HSS failed");
            }
        }
    }

    fn signal_of(def: &SeriesDef, leaf: &Leaf) -> Signal {
        Signal {
            id: def.series.id(),
            addr: leaf.place.addr,
            size: leaf.scalar.size,
            priority: def.priority,
            order: def.order,
        }
    }

    /// Rebuild the sampling tasks for the current set of series.
    fn replan(self: &Arc<Self>, state: &mut State) {
        self.stop_tasks(state);
        let Some(bound) = state.bound.clone() else {
            return;
        };
        let binding = &bound.binding;
        if state.is_off() {
            for def in &mut state.defs {
                def.actual = "off";
            }
            state.planned_rates = (0.0, 0.0);
            self.tsdb
                .push_event(EventKind::SamplingChanged, json!({"off": true}));
            return;
        }
        let config = state.effective(binding.config);

        // Which series may use which method.
        let mut hss_signals = Vec::new();
        let mut poll_ids = Vec::new();
        for def in &mut state.defs {
            def.actual = "none";
            let Some(leaf) = &def.resolved else { continue };
            let id = def.series.id();
            let region = binding.region_of(leaf.place.addr, leaf.scalar.size);
            if !binding.memory.is_empty() && region.is_none() {
                def.error = Some(format!(
                    "{:#010x} is not in known memory and may have read side effects",
                    leaf.place.addr
                ));
                continue;
            }
            let hss_allowed = state.hss_caps.is_some() && region != Some(false);
            match def.mode {
                Mode::Poll => poll_ids.push(id),
                Mode::Auto | Mode::Hss if hss_allowed => {
                    hss_signals.push(Self::signal_of(def, leaf));
                }
                Mode::Auto => poll_ids.push(id),
                Mode::Hss => def.error = Some("this address cannot use HSS".to_owned()),
            }
        }

        let mut hss_ids = Vec::new();
        if let (Some(caps), false) = (state.hss_caps, hss_signals.is_empty()) {
            let period = config
                .hss_period_us
                .max(1_000_000 / caps.max_freq_hz.max(1));
            let plan = plan_hss(
                &hss_signals,
                caps.max_blocks,
                hss_budget(binding.speed_khz, 1_000_000 / period.max(1)),
            );
            for id in &plan.demoted {
                let def = state
                    .defs
                    .iter_mut()
                    .find(|d| d.series.id() == *id)
                    .expect("planned signals come from the definitions");
                match def.mode {
                    Mode::Hss => def.error = Some("HSS capacity exceeded".to_owned()),
                    _ => poll_ids.push(*id),
                }
            }
            let blocks = plan.blocks.clone();
            let started = binding.handle.call_blocking(
                Priority::P0,
                move |probe: &mut dyn Probe| match probe.hss() {
                    Some(hss) => hss.hss_start(&blocks, period, HSS_FLAG_TIMESTAMP_US),
                    None => Err(ProbeError::Unsupported("HSS".to_owned())),
                },
            );
            match started {
                Ok(Ok(())) => {
                    state.hss_running = true;
                    let mut targets = Vec::with_capacity(plan.placements.len());
                    for (id, offset) in &plan.placements {
                        let def = state
                            .defs
                            .iter_mut()
                            .find(|d| d.series.id() == *id)
                            .expect("planned signals come from the definitions");
                        def.actual = "hss";
                        hss_ids.push(*id);
                        targets.push(HssTarget {
                            series: def.series.clone(),
                            offset: *offset as usize,
                            scalar: def.resolved.as_ref().expect("planned").scalar,
                        });
                    }
                    let mut reader = HssReader {
                        tsdb: self.tsdb.clone(),
                        counters: self.counters.clone(),
                        clock: ProbeClock::new(period),
                        record_size: plan.record_size(),
                        targets,
                        buffer: vec![0; HSS_BUFFER],
                        filled: 0,
                    };
                    state.planned_rates.0 = 1e6 / f64::from(period);
                    state.hss_task = binding
                        .handle
                        .periodic(Priority::P1, HSS_READ_INTERVAL, move |probe| {
                            reader.tick(probe)
                        })
                        .ok();
                }
                other => {
                    tracing::warn!("HSS did not start ({other:?}); polling instead");
                    poll_ids.extend(plan.placements.iter().map(|(id, _)| *id));
                }
            }
        }

        let poll_signals: Vec<Signal> = state
            .defs
            .iter()
            .filter(|d| poll_ids.contains(&d.series.id()))
            .filter_map(|d| d.resolved.as_ref().map(|leaf| Self::signal_of(d, leaf)))
            .collect();
        if !poll_signals.is_empty() {
            let ranges = plan_poll(&poll_signals);
            let groups = self.poll_groups(state, &ranges);
            for def in &mut state.defs {
                if poll_ids.contains(&def.series.id()) {
                    def.actual = "poll";
                }
            }
            let mut reader = PollReader {
                tsdb: self.tsdb.clone(),
                counters: self.counters.clone(),
                groups,
            };
            // Polled signals follow the chosen rate, up to what polling is for.
            let rate = (1_000_000 / config.hss_period_us.max(1)).clamp(1, MAX_POLL_HZ);
            state.planned_rates.1 = f64::from(rate);
            state.poll_task = binding
                .handle
                .periodic(
                    Priority::P3,
                    Duration::from_secs_f64(1.0 / f64::from(rate)),
                    move |probe| reader.tick(probe),
                )
                .ok();
        }

        self.tsdb.push_event(
            EventKind::SamplingChanged,
            json!({"hss": hss_ids.len(), "poll": poll_ids.len()}),
        );
    }

    fn poll_groups(&self, state: &State, ranges: &[PollRange]) -> Vec<PollGroup> {
        ranges
            .iter()
            .map(|range| PollGroup {
                addr: range.addr,
                bytes: vec![0; range.len as usize],
                targets: range
                    .placements
                    .iter()
                    .filter_map(|(id, offset)| {
                        let def = state.defs.iter().find(|d| d.series.id() == *id)?;
                        Some(PollTarget {
                            series: def.series.clone(),
                            offset: *offset as usize,
                            scalar: def.resolved.as_ref()?.scalar,
                        })
                    })
                    .collect(),
            })
            .collect()
    }

    fn status(state: &State, def: &SeriesDef) -> SeriesStatus {
        SeriesStatus {
            series_id: def.series.id(),
            name: def.series.meta().name,
            root: def.root.clone(),
            actual_mode: def.actual,
            rate_hz: match def.actual {
                "hss" => state.planned_rates.0,
                "poll" => state.planned_rates.1,
                _ => 0.0,
            },
            error: def.error.clone(),
        }
    }

    pub fn rate(&self) -> Rate {
        let state = self.state.lock().expect("sampler lock");
        let configured = state
            .bound
            .as_ref()
            .map_or_else(SamplingConfig::default, |bound| bound.binding.config);
        Rate {
            rate_hz: if state.is_off() {
                0
            } else {
                1_000_000 / state.effective(configured).hss_period_us.max(1)
            },
            max_hz: state.hss_caps.map_or(1000, |caps| caps.max_freq_hz),
        }
    }

    /// Change the rate of everything sampled for the Timeline. Zero stops
    /// sampling, leaving the series in place for when it starts again.
    pub fn set_rate(self: &Arc<Self>, hz: u32) -> Result<Rate, String> {
        {
            let mut state = self.state.lock().expect("sampler lock");
            let max = state.hss_caps.map_or(1000, |caps| caps.max_freq_hz);
            if hz != 0 && !(MIN_RATE_HZ..=max).contains(&hz) {
                return Err(format!(
                    "the rate must be off, or {MIN_RATE_HZ} to {max} Hz"
                ));
            }
            state.chosen_rate = Some(hz);
            self.replan(&mut state);
        }
        Ok(self.rate())
    }

    /// Start sampling an expression. Aggregates become one series per scalar.
    pub fn timeline_add(
        self: &Arc<Self>,
        expression: &str,
        mode: Mode,
        group: Option<String>,
        color: Option<String>,
        priority: i32,
    ) -> Result<Vec<SeriesStatus>, String> {
        let mut state = self.state.lock().expect("sampler lock");
        let bound = state.bound.clone().ok_or("no target is connected")?;
        let leaves = {
            let mut memory =
                HaltedMemory::with_priority(bound.binding.handle.clone(), Priority::P3);
            let rules = bound.rules.lock().expect("rules lock");
            resolve_leaves(&bound.binding.db, rules.as_ref(), expression, &mut memory)?
        };
        let mut created = Vec::with_capacity(leaves.len());
        for leaf in leaves {
            if state
                .defs
                .iter()
                .any(|d| d.root == expression && d.leaf_name == leaf.name)
            {
                continue;
            }
            let series = self
                .tsdb
                .create_series(leaf.name.clone(), group.clone(), color.clone());
            created.push(series.id());
            let order = state.next_order;
            state.next_order += 1;
            state.defs.push(SeriesDef {
                series,
                root: expression.to_owned(),
                leaf_name: leaf.name.clone(),
                mode,
                priority,
                order,
                resolved: Some(leaf),
                actual: "none",
                error: None,
            });
        }
        self.replan(&mut state);
        Ok(state
            .defs
            .iter()
            .filter(|d| created.contains(&d.series.id()))
            .map(|def| Self::status(&state, def))
            .collect())
    }

    pub fn timeline_remove(self: &Arc<Self>, series_id: u32) -> Result<(), String> {
        let mut state = self.state.lock().expect("sampler lock");
        let before = state.defs.len();
        state.defs.retain(|d| d.series.id() != series_id);
        if state.defs.len() == before {
            return Err(format!("no series {series_id}"));
        }
        self.tsdb.remove_series(series_id);
        self.replan(&mut state);
        Ok(())
    }

    pub fn timeline_list(&self) -> Vec<SeriesStatus> {
        let state = self.state.lock().expect("sampler lock");
        state
            .defs
            .iter()
            .map(|def| Self::status(&state, def))
            .collect()
    }

    pub fn status_json(&self) -> serde_json::Value {
        let state = self.state.lock().expect("sampler lock");
        match &state.bound {
            None => json!({"connected": false}),
            Some(bound) => json!({
                "connected": true,
                "device": bound.binding.device,
                "elf": bound.binding.elf,
                "speedKhz": bound.binding.speed_khz,
                "probe": bound.binding.probe,
                "hss": state.hss_caps.map(|caps| json!({
                    "maxBlocks": caps.max_blocks,
                    "maxFreqHz": caps.max_freq_hz,
                })),
                "hssRunning": state.hss_running,
            }),
        }
    }

    pub fn stats_json(&self) -> serde_json::Value {
        let load = |counter: &AtomicU64| counter.load(Ordering::Relaxed);
        let counters = &self.counters;
        let mut stats = json!({
            "hss": {
                "records": load(&counters.hss_records),
                "lost": load(&counters.hss_lost),
                "bytes": load(&counters.hss_bytes),
                "errors": load(&counters.hss_errors),
                "halts": load(&counters.hss_halts),
            },
            "poll": {
                "cycles": load(&counters.poll_cycles),
                "errors": load(&counters.poll_errors),
            },
        });
        if let Some(bound) = self.bound() {
            let names = ["P0", "P1", "P2", "P3", "P4"];
            let queues: Vec<serde_json::Value> = bound
                .binding
                .handle
                .stats()
                .iter()
                .zip(names)
                .map(|(queue, name): (&QueueStats, _)| {
                    json!({
                        "queue": name,
                        "executed": queue.executed,
                        "overruns": queue.overruns,
                        "maxDepth": queue.max_depth,
                        "p99Us": queue.run_time.p99_us,
                    })
                })
                .collect();
            stats["queues"] = json!(queues);
            let calls = bound
                .binding
                .handle
                .call_blocking(Priority::P4, |probe: &mut dyn Probe| probe.call_stats());
            if let Ok(calls) = calls {
                stats["calls"] = json!(calls.calls);
            }
        }
        stats
    }

    /// Write a scalar to the target.
    pub(crate) fn write_scalar(
        bound: &Bound,
        place: Place,
        scalar: Scalar,
        text: &str,
    ) -> Result<(), String> {
        engine::session::write_scalar(&bound.binding.handle, place, scalar, text)
    }

    pub fn note_event(&self, kind: EventKind, detail: serde_json::Value) {
        self.tsdb.push_event(kind, detail);
        if matches!(kind, EventKind::Halted) {
            self.watches.refresh_on_stop();
        }
    }

    /// The target has stopped, however briefly: show watches at their new values.
    pub fn note_stopped(&self) {
        self.watches.refresh_on_stop();
    }
}
