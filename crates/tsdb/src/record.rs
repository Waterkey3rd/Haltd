//! `.hlrec` recordings: a tagged stream of series metadata, sample blocks and
//! events, appended by a background thread so samplers never touch the disk.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crate::{write_csv_series, CsvColumn, CsvOptions, Event, SeriesMeta, Tsdb};

const MAGIC: &[u8; 8] = b"HLREC\x001\n";
/// Accepted when reading recordings written before the rename to Haltd.
const LEGACY_MAGIC: &[u8; 8] = b"JLREC\x001\n";
const TAG_SERIES: u8 = 1;
const TAG_SAMPLES: u8 = 2;
const TAG_EVENT: u8 = 3;
const FLUSH_INTERVAL: Duration = Duration::from_millis(200);
const CHUNK_LIMIT: usize = 1 << 16;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RecordingStats {
    pub samples: u64,
    /// Samples lost because the ring overwrote them before they were written.
    pub dropped: u64,
}

struct Counters {
    samples: AtomicU64,
    dropped: AtomicU64,
}

pub struct Recorder {
    path: PathBuf,
    started_ns: i64,
    stop: Arc<AtomicBool>,
    counters: Arc<Counters>,
    thread: Option<JoinHandle<io::Result<()>>>,
}

fn write_frame(out: &mut impl Write, tag: u8, payload: &[u8]) -> io::Result<()> {
    out.write_all(&[tag])?;
    out.write_all(&(payload.len() as u32).to_le_bytes())?;
    out.write_all(payload)
}

struct Writer {
    tsdb: Arc<Tsdb>,
    out: BufWriter<File>,
    cursors: BTreeMap<u32, u64>,
    last_event: u64,
    counters: Arc<Counters>,
}

impl Writer {
    fn flush_once(&mut self) -> io::Result<()> {
        for series in self.tsdb.list() {
            let id = series.id();
            if !self.cursors.contains_key(&id) {
                let meta = serde_json::to_vec(&series.meta()).map_err(io::Error::other)?;
                write_frame(&mut self.out, TAG_SERIES, &meta)?;
                // Only what arrives after the recording started belongs to it.
                self.cursors.insert(id, series.total());
                continue;
            }
            loop {
                let cursor = self.cursors[&id];
                let chunk = series.read_since(cursor, CHUNK_LIMIT);
                self.counters
                    .dropped
                    .fetch_add(chunk.dropped, Ordering::Relaxed);
                if chunk.t.is_empty() {
                    break;
                }
                let mut payload = Vec::with_capacity(8 + chunk.t.len() * 16);
                payload.extend_from_slice(&id.to_le_bytes());
                payload.extend_from_slice(&(chunk.t.len() as u32).to_le_bytes());
                for t in &chunk.t {
                    payload.extend_from_slice(&t.to_le_bytes());
                }
                for v in &chunk.v {
                    payload.extend_from_slice(&v.to_le_bytes());
                }
                write_frame(&mut self.out, TAG_SAMPLES, &payload)?;
                self.counters
                    .samples
                    .fetch_add(chunk.t.len() as u64, Ordering::Relaxed);
                self.cursors.insert(id, chunk.next);
            }
        }
        for event in self.tsdb.events_since(self.last_event) {
            self.last_event = event.seq;
            let payload = serde_json::to_vec(&event).map_err(io::Error::other)?;
            write_frame(&mut self.out, TAG_EVENT, &payload)?;
        }
        self.out.flush()
    }
}

impl Recorder {
    pub fn start(tsdb: Arc<Tsdb>, path: &Path) -> io::Result<Self> {
        let file = File::create(path)?;
        let mut out = BufWriter::new(file);
        out.write_all(MAGIC)?;
        let stop = Arc::new(AtomicBool::new(false));
        let counters = Arc::new(Counters {
            samples: AtomicU64::new(0),
            dropped: AtomicU64::new(0),
        });
        let started_ns = tsdb.now_ns();
        let last_event = tsdb.events_since(0).last().map_or(0, |event| event.seq);
        let mut writer = Writer {
            tsdb,
            out,
            cursors: BTreeMap::new(),
            last_event,
            counters: counters.clone(),
        };
        // Establish every series' starting point before returning.
        writer.flush_once()?;
        let flag = stop.clone();
        let thread = thread::Builder::new()
            .name("recorder".to_owned())
            .spawn(move || {
                while !flag.load(Ordering::SeqCst) {
                    writer.flush_once()?;
                    thread::sleep(FLUSH_INTERVAL);
                }
                writer.flush_once()
            })?;
        Ok(Self {
            path: path.to_owned(),
            started_ns,
            stop,
            counters,
            thread: Some(thread),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn started_ns(&self) -> i64 {
        self.started_ns
    }

    pub fn stats(&self) -> RecordingStats {
        RecordingStats {
            samples: self.counters.samples.load(Ordering::Relaxed),
            dropped: self.counters.dropped.load(Ordering::Relaxed),
        }
    }

    /// Write what remains and close the file.
    pub fn stop(mut self) -> io::Result<RecordingStats> {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            thread
                .join()
                .map_err(|_| io::Error::other("the recorder thread panicked"))??;
        }
        Ok(self.stats())
    }
}

impl Drop for Recorder {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[derive(Debug, Default)]
pub struct Recording {
    pub series: Vec<SeriesMeta>,
    pub samples: BTreeMap<u32, Vec<(i64, f64)>>,
    pub events: Vec<Event>,
}

pub fn read_recording(path: &Path) -> io::Result<Recording> {
    let mut input = BufReader::new(File::open(path)?);
    let mut magic = [0u8; 8];
    input.read_exact(&mut magic)?;
    if &magic != MAGIC && &magic != LEGACY_MAGIC {
        return Err(io::Error::other("not a haltd recording"));
    }
    let mut recording = Recording::default();
    loop {
        let mut header = [0u8; 5];
        match input.read_exact(&mut header) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => return Ok(recording),
            Err(error) => return Err(error),
        }
        let length = u32::from_le_bytes([header[1], header[2], header[3], header[4]]) as usize;
        let mut payload = vec![0u8; length];
        input.read_exact(&mut payload)?;
        match header[0] {
            TAG_SERIES => recording
                .series
                .push(serde_json::from_slice(&payload).map_err(io::Error::other)?),
            TAG_EVENT => recording
                .events
                .push(serde_json::from_slice(&payload).map_err(io::Error::other)?),
            TAG_SAMPLES => {
                let word = |at: usize| u32::from_le_bytes(payload[at..at + 4].try_into().unwrap());
                let (id, count) = (word(0), word(4) as usize);
                let times = payload[8..8 + count * 8].as_chunks::<8>().0;
                let values = payload[8 + count * 8..8 + count * 16].as_chunks::<8>().0;
                recording.samples.entry(id).or_default().extend(
                    times
                        .iter()
                        .zip(values)
                        .map(|(t, v)| (i64::from_le_bytes(*t), f64::from_le_bytes(*v))),
                );
            }
            // A newer writer may add frames this reader can skip.
            _ => {}
        }
    }
}

fn write_recording_csv(
    hlrec_path: &Path,
    only_names: Option<&[String]>,
    options: CsvOptions,
    out: impl Write,
) -> io::Result<u64> {
    let mut recording = read_recording(hlrec_path)?;
    let selected: Vec<SeriesMeta> = recording
        .series
        .into_iter()
        .filter(|meta| only_names.is_none_or(|wanted| wanted.iter().any(|name| name == &meta.name)))
        .collect();
    let names: Vec<String> = selected.iter().map(|meta| meta.name.clone()).collect();
    let columns: Vec<CsvColumn> = selected
        .iter()
        .map(|meta| CsvColumn {
            samples: recording.samples.remove(&meta.id).unwrap_or_default(),
            seed: None,
        })
        .collect();
    write_csv_series(&names, &columns, options.session_origin_ns, options, out)
}

/// Export an `.hlrec` recording file directly to CSV without relying on the
/// in-memory ring buffer.
pub fn export_recording_csv(
    hlrec_path: &Path,
    csv_path: &Path,
    only_names: Option<&[String]>,
    options: CsvOptions,
) -> io::Result<u64> {
    let out = BufWriter::new(File::create(csv_path)?);
    write_recording_csv(hlrec_path, only_names, options, out)
}

/// Export an `.hlrec` recording file into an in-memory CSV byte buffer.
pub fn export_recording_csv_bytes(
    hlrec_path: &Path,
    only_names: Option<&[String]>,
    options: CsvOptions,
) -> io::Result<(Vec<u8>, u64)> {
    let mut out = Vec::new();
    let rows = write_recording_csv(hlrec_path, only_names, options, &mut out)?;
    Ok((out, rows))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::EventKind;
    use serde_json::json;

    fn scratch(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("haltd-rec-{}-{name}", std::process::id()))
    }

    #[test]
    fn a_recording_holds_only_what_arrived_during_it() {
        let tsdb = Arc::new(Tsdb::default());
        let a = tsdb.create_series("a".to_owned(), None, None);
        a.push(1, 100.0);
        let path = scratch("during.hlrec");
        let recorder = Recorder::start(tsdb.clone(), &path).unwrap();
        a.push(2, 2.0);
        let b = tsdb.create_series("b".to_owned(), None, None);
        tsdb.push_event(EventKind::Halted, json!({"reason": "pause"}));
        thread::sleep(FLUSH_INTERVAL * 2);
        b.push(3, 3.0);
        let stats = recorder.stop().unwrap();
        assert_eq!(stats.samples, 2);
        assert_eq!(stats.dropped, 0);

        let recording = read_recording(&path).unwrap();
        assert_eq!(recording.series.len(), 2);
        assert_eq!(recording.samples[&a.id()], [(2, 2.0)]);
        assert_eq!(recording.samples[&b.id()], [(3, 3.0)]);
        assert_eq!(recording.events.len(), 1);
        assert_eq!(recording.events[0].kind, EventKind::Halted);
    }

    #[test]
    fn other_files_are_rejected() {
        let path = scratch("bad.hlrec");
        std::fs::write(&path, b"not this format").unwrap();
        assert!(read_recording(&path).is_err());
    }

    #[test]
    fn a_tail_cut_inside_a_frame_is_an_error() {
        let tsdb = Arc::new(Tsdb::default());
        let a = tsdb.create_series("a".to_owned(), None, None);
        let path = scratch("cut.hlrec");
        let recorder = Recorder::start(tsdb.clone(), &path).unwrap();
        a.push(1, 1.0);
        recorder.stop().unwrap();
        let bytes = std::fs::read(&path).unwrap();
        std::fs::write(&path, &bytes[..bytes.len() - 3]).unwrap();
        assert!(read_recording(&path).is_err());
    }

    #[test]
    fn legacy_jlrec_recordings_still_load() {
        let tsdb = Arc::new(Tsdb::default());
        let a = tsdb.create_series("a".to_owned(), None, None);
        let path = scratch("legacy.jlrec");
        let recorder = Recorder::start(tsdb.clone(), &path).unwrap();
        a.push(1, 4.5);
        recorder.stop().unwrap();

        let mut bytes = std::fs::read(&path).unwrap();
        assert_eq!(&bytes[..MAGIC.len()], MAGIC);
        bytes[..LEGACY_MAGIC.len()].copy_from_slice(LEGACY_MAGIC);
        std::fs::write(&path, &bytes).unwrap();

        let recording = read_recording(&path).unwrap();
        assert_eq!(recording.samples[&a.id()], [(1, 4.5)]);
    }

    #[test]
    fn recording_exports_directly_to_csv_with_zoh_and_filter() {
        let tsdb = Arc::new(Tsdb::default());
        let a = tsdb.create_series("sig_a".to_owned(), None, None);
        let b = tsdb.create_series("sig_b".to_owned(), None, None);
        let hlrec = scratch("export.hlrec");
        let recorder = Recorder::start(tsdb.clone(), &hlrec).unwrap();
        a.push(1_000_000_000, 5.0);
        b.push(2_000_000_000, 9.0);
        thread::sleep(FLUSH_INTERVAL * 2);
        recorder.stop().unwrap();

        let csv_path = scratch("export.csv");
        let rows = export_recording_csv(
            &hlrec,
            &csv_path,
            Some(&["sig_a".to_owned(), "sig_b".to_owned()]),
            CsvOptions {
                zero_order_hold: true,
                relative_time: true,
                session_origin_ns: 0,
            },
        )
        .unwrap();
        assert_eq!(rows, 2);
        assert_eq!(
            std::fs::read_to_string(&csv_path).unwrap(),
            "t_s,sig_a,sig_b\n0.000000000,5,\n1.000000000,5,9\n"
        );
    }
}
