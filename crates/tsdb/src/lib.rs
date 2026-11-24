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

pub use record::{read_recording, Recorder, Recording, RecordingStats};
pub use ring::{Chunk, Ring};

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

pub struct Series {
    meta: RwLock<SeriesMeta>,
    data: Mutex<Ring>,
}

impl Series {
    pub fn id(&self) -> u32 {
        self.meta.read().expect("meta lock").id
    }

    pub fn meta(&self) -> SeriesMeta {
        self.meta.read().expect("meta lock").clone()
    }

    pub fn push(&self, t: i64, v: f64) {
        self.data.lock().expect("ring lock").push(t, v);
    }

    /// Append many samples under one lock, for high-rate writers.
    pub fn push_all(&self, samples: impl IntoIterator<Item = (i64, f64)>) {
        let mut ring = self.data.lock().expect("ring lock");
        for (t, v) in samples {
            ring.push(t, v);
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
}

pub struct Tsdb {
    series: RwLock<BTreeMap<u32, Arc<Series>>>,
    events: Mutex<VecDeque<Event>>,
    next_id: AtomicU32,
    next_event: AtomicU64,
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
            started: Instant::now(),
            started_unix_ns: unix,
            capacity,
        }
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
        let mut events = self.events.lock().expect("event lock");
        if events.len() == MAX_EVENTS {
            events.pop_front();
        }
        events.push_back(event);
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
        let series: Vec<Arc<Series>> = ids.iter().filter_map(|id| self.get(*id)).collect();
        let mut columns: Vec<Vec<(i64, f64)>> = Vec::with_capacity(series.len());
        for item in &series {
            let chunk = item.read_since(0, usize::MAX >> 1);
            columns.push(
                chunk
                    .t
                    .into_iter()
                    .zip(chunk.v)
                    .filter(|(t, _)| *t >= t0 && *t < t1)
                    .collect(),
            );
        }
        let mut out = BufWriter::new(File::create(path)?);
        write!(out, "t_s")?;
        for item in &series {
            write!(out, ",{}", item.meta().name.replace(',', ";"))?;
        }
        writeln!(out)?;

        let origin = columns
            .iter()
            .filter_map(|column| column.first().map(|(t, _)| *t))
            .min()
            .unwrap_or(t0);
        let mut positions = vec![0usize; columns.len()];
        let mut rows = 0;
        loop {
            let next = columns
                .iter()
                .zip(&positions)
                .filter_map(|(column, position)| column.get(*position).map(|(t, _)| *t))
                .min();
            let Some(time) = next else { break };
            write!(out, "{:.9}", (time - origin) as f64 / 1e9)?;
            for (column, position) in columns.iter().zip(&mut positions) {
                match column.get(*position) {
                    Some((t, v)) if *t == time => {
                        write!(out, ",{v}")?;
                        *position += 1;
                    }
                    _ => write!(out, ",")?,
                }
            }
            writeln!(out)?;
            rows += 1;
        }
        out.flush()?;
        Ok(rows)
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
}
