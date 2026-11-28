//! Time-series storage shared by every data source (DESIGN §5.9).

mod record;
mod ring;

use std::collections::{BTreeMap, VecDeque};
use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::path::Path;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

pub use record::{
    export_recording_csv, export_recording_csv_bytes, read_recording, Recorder, Recording,
    RecordingStats,
};
pub use ring::{Chunk, RawWindow, Ring};

/// Options controlling CSV export alignment and time column origin.
#[derive(Clone, Copy, Debug)]
pub struct CsvOptions {
    /// Fill missing timestamps on mixed-rate series with each column's last known sample.
    pub zero_order_hold: bool,
    /// Shift `t_s` so the earliest exported sample sits at `0.000000000`.
    pub relative_time: bool,
    /// Reference epoch subtracted when `relative_time` is `false`.
    pub session_origin_ns: i64,
}

impl Default for CsvOptions {
    fn default() -> Self {
        Self {
            zero_order_hold: false,
            relative_time: true,
            session_origin_ns: 0,
        }
    }
}

pub(crate) struct CsvColumn {
    pub samples: Vec<(i64, f64)>,
    pub seed: Option<f64>,
}

pub(crate) fn write_csv_series(
    names: &[String],
    columns: &[CsvColumn],
    fallback_origin: i64,
    options: CsvOptions,
    mut out: impl Write,
) -> io::Result<u64> {
    write!(out, "t_s")?;
    for name in names {
        write!(out, ",{}", name.replace(',', ";"))?;
    }
    writeln!(out)?;

    let origin = if options.relative_time {
        columns
            .iter()
            .filter_map(|column| column.samples.first().map(|(t, _)| *t))
            .min()
            .unwrap_or(fallback_origin)
    } else {
        options.session_origin_ns
    };

    let mut positions = vec![0usize; columns.len()];
    let mut held: Vec<Option<f64>> = columns.iter().map(|c| c.seed).collect();
    let mut rows = 0;
    loop {
        let next = columns
            .iter()
            .zip(&positions)
            .filter_map(|(column, position)| column.samples.get(*position).map(|(t, _)| *t))
            .min();
        let Some(time) = next else { break };
        write!(out, "{:.9}", (time - origin) as f64 / 1e9)?;
        for (idx, (column, position)) in columns.iter().zip(&mut positions).enumerate() {
            match column.samples.get(*position) {
                Some((t, v)) if *t == time => {
                    write!(out, ",{v}")?;
                    held[idx] = Some(*v);
                    *position += 1;
                }
                _ => match held[idx].filter(|_| options.zero_order_hold) {
                    Some(v) => write!(out, ",{v}")?,
                    None => write!(out, ",")?,
                },
            }
        }
        writeln!(out)?;
        rows += 1;
    }
    out.flush()?;
    Ok(rows)
}

/// 1 kHz for ten minutes.
pub const DEFAULT_CAPACITY: usize = 600_000;
const MAX_EVENTS: usize = 10_000;

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SeriesMeta {
    pub id: u32,
    pub name: String,
    pub group: Option<String>,
    pub color: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[repr(u16)]
pub enum EventKind {
    Halted = 1,
    Resumed = 2,
    Reset = 3,
    Reload = 4,
    SamplingChanged = 5,
    Fault = 6,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Event {
    pub seq: u64,
    pub t: i64,
    pub kind: EventKind,
    pub detail: serde_json::Value,
}

type ChangeListener = Arc<dyn Fn() + Send + Sync>;

struct ChangeHub {
    revision: AtomicU64,
    listeners: RwLock<Vec<ChangeListener>>,
}

impl ChangeHub {
    fn new() -> Self {
        Self {
            revision: AtomicU64::new(1),
            listeners: RwLock::new(Vec::new()),
        }
    }

    fn revision(&self) -> u64 {
        self.revision.load(Ordering::Acquire)
    }

    fn bump(&self) {
        self.revision.fetch_add(1, Ordering::Release);
        let listeners = self.listeners.read().unwrap_or_else(|err| err.into_inner());
        for listener in listeners.iter() {
            listener();
        }
    }

    fn subscribe(&self, listener: ChangeListener) {
        self.listeners
            .write()
            .unwrap_or_else(|err| err.into_inner())
            .push(listener);
    }
}

pub struct Series {
    meta: RwLock<SeriesMeta>,
    data: Mutex<Ring>,
    hub: Arc<ChangeHub>,
}

impl Series {
    pub fn id(&self) -> u32 {
        self.meta.read().expect("meta lock").id
    }

    pub fn meta(&self) -> SeriesMeta {
        self.meta.read().expect("meta lock").clone()
    }

    pub fn push(&self, t: i64, v: f64) {
        {
            self.data.lock().expect("ring lock").push(t, v);
        }
        self.hub.bump();
    }

    /// Append many samples under one lock, for high-rate writers.
    pub fn push_all(&self, samples: impl IntoIterator<Item = (i64, f64)>) {
        let grew = {
            let mut ring = self.data.lock().expect("ring lock");
            let before = ring.total();
            for (t, v) in samples {
                ring.push(t, v);
            }
            ring.total() > before
        };
        if grew {
            self.hub.bump();
        }
    }

    pub fn read_since(&self, cursor: u64, limit: usize) -> Chunk {
        self.data
            .lock()
            .expect("ring lock")
            .read_since(cursor, limit)
    }

    pub fn total(&self) -> u64 {
        self.data.lock().expect("ring lock").total()
    }

    pub fn latest(&self) -> Option<(i64, f64)> {
        let ring = self.data.lock().expect("ring lock");
        let last = ring.total().checked_sub(1)?;
        Some((ring.time_at(last), ring.value_at(last)))
    }

    pub fn columns(&self, t0: i64, t1: i64, min: &mut [f64], max: &mut [f64]) {
        self.data
            .lock()
            .expect("ring lock")
            .columns(t0, t1, min, max);
    }

    pub fn raw_window(&self, t0: i64, t1: i64, max_points: usize) -> Option<RawWindow> {
        self.data
            .lock()
            .expect("ring lock")
            .raw_window(t0, t1, max_points)
    }
}

pub struct Tsdb {
    series: RwLock<BTreeMap<u32, Arc<Series>>>,
    events: Mutex<VecDeque<Event>>,
    next_id: AtomicU32,
    next_event: AtomicU64,
    hub: Arc<ChangeHub>,
    started: Instant,
    started_unix_ns: i64,
    capacity: usize,
}

impl Default for Tsdb {
    fn default() -> Self {
        Self::with_capacity(DEFAULT_CAPACITY)
    }
}

impl Tsdb {
    pub fn with_capacity(capacity: usize) -> Self {
        let unix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_nanos() as i64);
        Self {
            series: RwLock::new(BTreeMap::new()),
            events: Mutex::new(VecDeque::new()),
            next_id: AtomicU32::new(1),
            next_event: AtomicU64::new(1),
            hub: Arc::new(ChangeHub::new()),
            started: Instant::now(),
            started_unix_ns: unix,
            capacity,
        }
    }

    pub fn revision(&self) -> u64 {
        self.hub.revision()
    }

    pub fn on_change(&self, listener: ChangeListener) {
        self.hub.subscribe(listener);
    }

    /// Host timeline: nanoseconds since the Unix epoch, driven by a monotonic
    /// clock so that wall-clock adjustments cannot bend it.
    pub fn now_ns(&self) -> i64 {
        self.started_unix_ns + self.started.elapsed().as_nanos() as i64
    }

    pub fn create_series(
        &self,
        name: String,
        group: Option<String>,
        color: Option<String>,
    ) -> Arc<Series> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let series = Arc::new(Series {
            meta: RwLock::new(SeriesMeta {
                id,
                name,
                group,
                color,
            }),
            data: Mutex::new(Ring::new(self.capacity)),
            hub: Arc::clone(&self.hub),
        });
        self.series
            .write()
            .expect("series lock")
            .insert(id, series.clone());
        series
    }

    pub fn remove_series(&self, id: u32) -> bool {
        self.series
            .write()
            .expect("series lock")
            .remove(&id)
            .is_some()
    }

    pub fn get(&self, id: u32) -> Option<Arc<Series>> {
        self.series.read().expect("series lock").get(&id).cloned()
    }

    pub fn list(&self) -> Vec<Arc<Series>> {
        self.series
            .read()
            .expect("series lock")
            .values()
            .cloned()
            .collect()
    }

    pub fn update_meta(&self, id: u32, group: Option<String>, color: Option<String>) -> bool {
        let Some(series) = self.get(id) else {
            return false;
        };
        let mut meta = series.meta.write().expect("meta lock");
        if group.is_some() {
            meta.group = group;
        }
        if color.is_some() {
            meta.color = color;
        }
        true
    }

    pub fn push_event(&self, kind: EventKind, detail: serde_json::Value) {
        let event = Event {
            seq: self.next_event.fetch_add(1, Ordering::Relaxed),
            t: self.now_ns(),
            kind,
            detail,
        };
        {
            let mut events = self.events.lock().expect("event lock");
            if events.len() == MAX_EVENTS {
                events.pop_front();
            }
            events.push_back(event);
        }
        self.hub.bump();
    }

    /// Events with a sequence number above `after`.
    pub fn events_since(&self, after: u64) -> Vec<Event> {
        let events = self.events.lock().expect("event lock");
        let start = events.partition_point(|event| event.seq <= after);
        events.iter().skip(start).cloned().collect()
    }

    pub fn events_in(&self, t0: i64, t1: i64) -> Vec<Event> {
        self.events
            .lock()
            .expect("event lock")
            .iter()
            .filter(|event| event.t >= t0 && event.t < t1)
            .cloned()
            .collect()
    }

    /// Write `[t0, t1)` as CSV, one row per distinct timestamp. Series sampled
    /// together share rows; the others leave their columns blank.
    pub fn export_csv(&self, ids: &[u32], t0: i64, t1: i64, path: &Path) -> io::Result<u64> {
        self.export_csv_with(ids, t0, t1, CsvOptions::default(), path)
    }

    pub fn export_csv_with(
        &self,
        ids: &[u32],
        t0: i64,
        t1: i64,
        options: CsvOptions,
        path: &Path,
    ) -> io::Result<u64> {
        let out = BufWriter::new(File::create(path)?);
        self.write_export(ids, t0, t1, options, out)
    }

    pub fn export_csv_bytes(
        &self,
        ids: &[u32],
        t0: i64,
        t1: i64,
        options: CsvOptions,
    ) -> io::Result<(Vec<u8>, u64)> {
        let mut out = Vec::new();
        let rows = self.write_export(ids, t0, t1, options, &mut out)?;
        Ok((out, rows))
    }

    fn write_export(
        &self,
        ids: &[u32],
        t0: i64,
        t1: i64,
        options: CsvOptions,
        out: impl Write,
    ) -> io::Result<u64> {
        let series: Vec<Arc<Series>> = ids.iter().filter_map(|id| self.get(*id)).collect();
        let names: Vec<String> = series.iter().map(|item| item.meta().name).collect();
        let mut columns = Vec::with_capacity(series.len());
        for item in &series {
            let chunk = item.read_since(0, usize::MAX >> 1);
            let mut seed = None;
            let mut samples = Vec::new();
            for (t, v) in chunk.t.into_iter().zip(chunk.v) {
                if t < t0 {
                    seed = Some(v);
                } else if t < t1 {
                    samples.push((t, v));
                }
            }
            columns.push(CsvColumn { samples, seed });
        }
        write_csv_series(&names, &columns, t0, options, out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn events_are_ordered_and_bounded_by_cursor() {
        let db = Tsdb::default();
        db.push_event(EventKind::Halted, json!({"reason": "breakpoint"}));
        db.push_event(EventKind::Resumed, json!({}));
        let all = db.events_since(0);
        assert_eq!(all.len(), 2);
        assert!(all[0].t <= all[1].t);
        assert_eq!(db.events_since(all[0].seq).len(), 1);
        assert!(db.events_since(all[1].seq).is_empty());
    }

    #[test]
    fn series_can_be_listed_updated_and_removed() {
        let db = Tsdb::default();
        let a = db.create_series("a".to_owned(), None, None);
        db.create_series("b".to_owned(), Some("g".to_owned()), None);
        assert_eq!(db.list().len(), 2);
        assert!(db.update_meta(a.id(), Some("x".to_owned()), None));
        assert_eq!(a.meta().group.as_deref(), Some("x"));
        assert!(db.remove_series(a.id()));
        assert!(db.get(a.id()).is_none());
        assert!(!db.remove_series(a.id()));
    }

    fn scratch(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("haltd-tsdb-{}-{name}", std::process::id()))
    }

    #[test]
    fn csv_shares_rows_for_equal_timestamps() {
        let db = Tsdb::default();
        let a = db.create_series("a".to_owned(), None, None);
        let b = db.create_series("b,c".to_owned(), None, None);
        a.push(1_000_000_000, 1.5);
        b.push(1_000_000_000, 2.5);
        a.push(2_000_000_000, 3.5);
        let path = scratch("rows.csv");
        let rows = db
            .export_csv(&[a.id(), b.id()], 0, i64::MAX, &path)
            .unwrap();
        assert_eq!(rows, 2);
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "t_s,a,b;c\n0.000000000,1.5,2.5\n1.000000000,3.5,\n"
        );
    }

    /// Acceptance load: `cargo test --release -p tsdb -- --ignored --nocapture`.
    #[test]
    #[ignore = "allocates about 200 MB"]
    fn full_rings_answer_queries_quickly() {
        let db = Tsdb::default();
        let series: Vec<_> = (0..16)
            .map(|i| db.create_series(format!("s{i}"), None, None))
            .collect();
        for item in &series {
            item.push_all((0..DEFAULT_CAPACITY as i64).map(|i| (i * 1_000_000, (i as f64).sin())));
        }
        let (mut min, mut max) = (vec![0.0; 1920], vec![0.0; 1920]);
        let span = DEFAULT_CAPACITY as i64 * 1_000_000;
        for (label, t0, t1) in [
            ("everything", 0, span),
            ("half", span / 4, span * 3 / 4),
            ("five seconds", span / 2, span / 2 + 5_000_000_000),
        ] {
            let started = Instant::now();
            for item in &series {
                item.columns(t0, t1, &mut min, &mut max);
            }
            let elapsed = started.elapsed();
            println!("{label}: {elapsed:.2?} for 16 series x 1920 columns");
            assert!(elapsed.as_millis() < 16, "{label} took {elapsed:?}");
        }
    }

    #[test]
    fn csv_honours_the_time_range() {
        let db = Tsdb::default();
        let a = db.create_series("a".to_owned(), None, None);
        for i in 0..10 {
            a.push(i, i as f64);
        }
        let path = scratch("range.csv");
        assert_eq!(db.export_csv(&[a.id()], 3, 6, &path).unwrap(), 3);
    }

    #[test]
    fn csv_zoh_fills_missing_columns_and_keeps_session_origin() {
        let db = Tsdb::default();
        let a = db.create_series("a".to_owned(), None, None);
        let b = db.create_series("b".to_owned(), None, None);
        a.push(500_000_000, 10.0);
        b.push(1_000_000_000, 1.0);
        a.push(2_000_000_000, 20.0);

        let (bytes, rows) = db
            .export_csv_bytes(
                &[a.id(), b.id()],
                1_000_000_000,
                3_000_000_000,
                CsvOptions {
                    zero_order_hold: true,
                    relative_time: false,
                    session_origin_ns: 0,
                },
            )
            .unwrap();
        assert_eq!(rows, 2);
        assert_eq!(
            String::from_utf8(bytes).unwrap(),
            "t_s,a,b\n1.000000000,10,1\n2.000000000,20,1\n"
        );
    }

    #[test]
    fn revision_increments_on_samples_and_events_and_exposes_raw_window() {
        let db = Tsdb::default();
        let r0 = db.revision();
        let s = db.create_series("x".to_owned(), None, None);
        s.push(100, 1.0);
        let r1 = db.revision();
        assert!(r1 > r0);
        s.push_all([(200, 2.0), (300, 3.0)]);
        let r2 = db.revision();
        assert!(r2 > r1);
        db.push_event(EventKind::Halted, json!({}));
        assert!(db.revision() > r2);

        let raw = s.raw_window(150, 250, 8).expect("raw window");
        assert_eq!(raw.t, [100, 200, 300]);
        assert_eq!(raw.v, [1.0, 2.0, 3.0]);
    }

    #[test]
    fn change_listener_fires_on_push_push_all_and_events() {
        let db = Tsdb::default();
        let hits = Arc::new(AtomicU64::new(0));
        let hits_cb = Arc::clone(&hits);
        db.on_change(Arc::new(move || {
            hits_cb.fetch_add(1, Ordering::Relaxed);
        }));
        let s = db.create_series("sig".to_owned(), None, None);
        s.push(100, 1.0);
        assert_eq!(hits.load(Ordering::Relaxed), 1);

        s.push_all(std::iter::empty());
        assert_eq!(hits.load(Ordering::Relaxed), 1);

        s.push_all([(200, 2.0), (300, 3.0)]);
        assert_eq!(hits.load(Ordering::Relaxed), 2);

        db.push_event(EventKind::Halted, json!({}));
        assert_eq!(hits.load(Ordering::Relaxed), 3);
    }
}
