//! RTT engine: finds the control block, reads channels on the probe thread and
//! decodes them on a worker thread (DESIGN §5.8).

pub mod config;
pub mod decode;

use std::collections::{BTreeMap, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use config::{ChannelConfig, Decode, RttConfig};
use decode::{Decoder, Sink, TextDecoder};
use probe_api::{AccessWidth, Probe, ProbeError, RttDirection};
use probe_jlink::actor::{PeriodicId, Priority, ProbeHandle};
use sampler::{BindListener, Binding};
use serde::Serialize;
use tsdb::{Series, Tsdb};

const READ_INTERVAL: Duration = Duration::from_millis(5);
const READ_CHUNK: usize = 8 * 1024;
const SEARCH_INTERVAL: Duration = Duration::from_millis(100);
const TERMINAL_HISTORY: usize = 64 * 1024;
const STOP_POLL: Duration = Duration::from_millis(50);
const CONTROL_BLOCK_ID: [u8; 10] = *b"SEGGER RTT";
/// Anything larger means the descriptor was read from stale memory.
const MAX_CHANNEL_SIZE: u32 = 16 << 20;

#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ChannelStatus {
    pub index: u32,
    pub name: String,
    pub direction: &'static str,
    pub size: u32,
    pub decoder: &'static str,
    pub bytes: u64,
    pub frames: u64,
    pub lost: u64,
    pub resyncs: u64,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SignalInfo {
    pub name: String,
    pub value: Option<f64>,
    pub series_id: Option<u32>,
}

#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RttStatus {
    /// `idle`, `searching` or `running`.
    pub state: &'static str,
    pub control_block: Option<u32>,
    pub error: Option<String>,
}

#[derive(Default)]
struct Terminal {
    bytes: VecDeque<u8>,
    total: u64,
}

impl Terminal {
    fn push(&mut self, data: &[u8]) {
        self.bytes.extend(data);
        self.total += data.len() as u64;
        let excess = self.bytes.len().saturating_sub(TERMINAL_HISTORY);
        self.bytes.drain(..excess);
    }

    fn read_since(&self, cursor: u64) -> (Vec<u8>, u64) {
        let start = self.total - self.bytes.len() as u64;
        let from = (cursor.max(start) - start) as usize;
        (self.bytes.iter().skip(from).copied().collect(), self.total)
    }
}

struct Signal {
    series: Option<Arc<Series>>,
    group: Option<String>,
    /// The user took this off the Timeline; do not bring it back by itself.
    removed: bool,
    latest: Option<f64>,
}

#[derive(Default)]
struct Shared {
    status: Mutex<RttStatus>,
    channels: Mutex<Vec<ChannelStatus>>,
    terminals: Mutex<BTreeMap<u32, Terminal>>,
    signals: Mutex<BTreeMap<String, Signal>>,
    /// Timeline sampling is off: decode and show values, but record nothing.
    paused: AtomicBool,
}

struct Running {
    stop: Arc<AtomicBool>,
    thread: JoinHandle<()>,
    handle: ProbeHandle,
}

pub struct Rtt {
    tsdb: Arc<Tsdb>,
    shared: Arc<Shared>,
    running: Mutex<Option<Running>>,
}

struct Chunk {
    channel: u32,
    data: Vec<u8>,
    t: i64,
}

struct ChannelSink<'a> {
    shared: &'a Shared,
    tsdb: &'a Tsdb,
    channel: u32,
}

impl Sink for ChannelSink<'_> {
    fn sample(&mut self, name: &str, group: Option<&str>, plot: bool, t: i64, value: f64) {
        let mut signals = self.shared.signals.lock().expect("signal lock");
        let signal = signals.entry(name.to_owned()).or_insert_with(|| Signal {
            series: None,
            group: group.map(str::to_owned),
            removed: false,
            latest: None,
        });
        signal.latest = Some(value);
        if signal.series.is_none() && plot && !signal.removed {
            signal.series = Some(self.tsdb.create_series(
                name.to_owned(),
                signal.group.clone(),
                None,
            ));
        }
        if let Some(series) = signal
            .series
            .as_ref()
            .filter(|_| !self.shared.paused.load(Ordering::Relaxed))
        {
            series.push(t, value);
        }
    }

    fn text(&mut self, data: &[u8]) {
        self.shared
            .terminals
            .lock()
            .expect("terminal lock")
            .entry(self.channel)
            .or_default()
            .push(data);
    }
}

fn decoder_name(config: Option<&ChannelConfig>) -> &'static str {
    match config.map(|channel| &channel.decode) {
        None | Some(Decode::Text { .. }) => "text",
        Some(Decode::TextRegex { .. }) => "text-regex",
        Some(Decode::Struct(_)) => "struct",
        Some(Decode::Lua { .. }) => "lua",
        Some(Decode::Systemview { .. }) => "systemview",
    }
}

fn load_config(binding: &Binding) -> Result<RttConfig, String> {
    match &binding.rtt_config {
        Some(path) => RttConfig::load(path),
        None => Ok(RttConfig::default()),
    }
}

impl Rtt {
    pub fn new(tsdb: Arc<Tsdb>) -> Arc<Self> {
        Arc::new(Self {
            tsdb,
            shared: Arc::default(),
            running: Mutex::new(None),
        })
    }

    pub fn set_paused(&self, paused: bool) {
        self.shared.paused.store(paused, Ordering::Relaxed);
    }

    pub fn status(&self) -> RttStatus {
        self.shared.status.lock().expect("status lock").clone()
    }

    pub fn channels(&self) -> Vec<ChannelStatus> {
        self.shared.channels.lock().expect("channel lock").clone()
    }

    pub fn signals(&self) -> Vec<SignalInfo> {
        self.shared
            .signals
            .lock()
            .expect("signal lock")
            .iter()
            .map(|(name, signal)| SignalInfo {
                name: name.clone(),
                value: signal.latest,
                series_id: signal.series.as_ref().map(|series| series.id()),
            })
            .collect()
    }

    /// Start plotting a decoded signal.
    pub fn plot_signal(&self, name: &str, group: Option<String>) -> Result<u32, String> {
        let mut signals = self.shared.signals.lock().expect("signal lock");
        let signal = signals
            .get_mut(name)
            .ok_or_else(|| format!("no RTT signal named {name:?}"))?;
        signal.removed = false;
        if let Some(group) = group {
            signal.group = Some(group);
        }
        if signal.series.is_none() {
            signal.series = Some(self.tsdb.create_series(
                name.to_owned(),
                signal.group.clone(),
                None,
            ));
        }
        Ok(signal.series.as_ref().expect("just set").id())
    }

    /// Stop plotting; returns false if this series is not an RTT signal.
    pub fn remove_series(&self, id: u32) -> bool {
        let mut signals = self.shared.signals.lock().expect("signal lock");
        for signal in signals.values_mut() {
            if signal
                .series
                .as_ref()
                .is_some_and(|series| series.id() == id)
            {
                signal.series = None;
                signal.removed = true;
                self.tsdb.remove_series(id);
                return true;
            }
        }
        false
    }

    /// Series currently fed by RTT, as `(id, name)`.
    pub fn plotted(&self) -> Vec<(u32, String)> {
        self.shared
            .signals
            .lock()
            .expect("signal lock")
            .iter()
            .filter_map(|(name, signal)| {
                signal
                    .series
                    .as_ref()
                    .map(|series| (series.id(), name.clone()))
            })
            .collect()
    }

    pub fn terminal_cursor(&self, channel: u32) -> u64 {
        let terminals = self.shared.terminals.lock().expect("terminal lock");
        terminals
            .get(&channel)
            .map_or(0, |terminal| terminal.total - terminal.bytes.len() as u64)
    }

    pub fn terminal_read(&self, channel: u32, cursor: u64) -> Option<(Vec<u8>, u64)> {
        let terminals = self.shared.terminals.lock().expect("terminal lock");
        let (data, next) = terminals.get(&channel)?.read_since(cursor);
        (!data.is_empty()).then_some((data, next))
    }

    /// Send bytes to the target on a down channel.
    pub fn write(&self, channel: u32, data: Vec<u8>) -> Result<usize, String> {
        let running = self.running.lock().expect("running lock");
        let handle = &running.as_ref().ok_or("RTT is not running")?.handle;
        handle
            .call_blocking(Priority::P0, move |probe: &mut dyn Probe| {
                match probe.rtt() {
                    Some(rtt) => rtt.rtt_write(channel, &data),
                    None => Err(ProbeError::Unsupported("RTT".to_owned())),
                }
            })
            .map_err(|e| e.to_string())?
            .map_err(|e| e.to_string())
    }

    fn stop(&self) {
        let running = self.running.lock().expect("running lock").take();
        if let Some(running) = running {
            running.stop.store(true, Ordering::SeqCst);
            let _ = running.thread.join();
        }
        *self.shared.status.lock().expect("status lock") = RttStatus {
            state: "idle",
            ..RttStatus::default()
        };
    }

    fn start(self: &Arc<Self>, binding: &Binding) {
        self.stop();
        let stop = Arc::new(AtomicBool::new(false));
        let worker = Worker {
            tsdb: self.tsdb.clone(),
            shared: self.shared.clone(),
            binding: binding.clone(),
            stop: stop.clone(),
        };
        let handle = binding.handle.clone();
        let thread = thread::Builder::new()
            .name("rtt".to_owned())
            .spawn(move || worker.run())
            .expect("spawning the RTT thread");
        *self.running.lock().expect("running lock") = Some(Running {
            stop,
            thread,
            handle,
        });
    }
}

impl BindListener for Rtt {
    fn bound(self: Arc<Self>, binding: &Binding) {
        self.start(binding);
    }

    fn unbound(self: Arc<Self>) {
        self.stop();
    }
}

struct Worker {
    tsdb: Arc<Tsdb>,
    shared: Arc<Shared>,
    binding: Binding,
    stop: Arc<AtomicBool>,
}

impl Worker {
    fn set_status(&self, state: &'static str, control_block: Option<u32>, error: Option<String>) {
        *self.shared.status.lock().expect("status lock") = RttStatus {
            state,
            control_block,
            error,
        };
    }

    fn call<R, F>(&self, f: F) -> Result<R, String>
    where
        R: Send + 'static,
        F: FnOnce(&mut dyn probe_api::RttApi) -> probe_api::Result<R> + Send + 'static,
    {
        self.binding
            .handle
            .call_blocking(Priority::P4, move |probe: &mut dyn Probe| {
                match probe.rtt() {
                    Some(rtt) => f(rtt),
                    None => Err(ProbeError::Unsupported("RTT".to_owned())),
                }
            })
            .map_err(|e| e.to_string())?
            .map_err(|e| e.to_string())
    }

    /// Wait until firmware has written the control block's ID. The library
    /// trusts an explicit address, so it must not look before then.
    fn wait_for_id(&self, address: u32) -> bool {
        while !self.stop.load(Ordering::SeqCst) {
            let read =
                self.binding
                    .handle
                    .call_blocking(Priority::P4, move |probe: &mut dyn Probe| {
                        let mut id = [0u8; CONTROL_BLOCK_ID.len()];
                        probe
                            .read_mem(address, &mut id, AccessWidth::U8)
                            .map(|()| id)
                    });
            if matches!(read, Ok(Ok(id)) if id == CONTROL_BLOCK_ID) {
                return true;
            }
            thread::sleep(SEARCH_INTERVAL);
        }
        false
    }

    /// Wait for the firmware to initialize its control block. After a restart
    /// the library reports the old channel count at once but descriptors stay
    /// empty until it has read the block again, so a real size is the signal.
    fn wait_for_channels(&self, config: &RttConfig) -> Option<Vec<ChannelStatus>> {
        loop {
            if self.stop.load(Ordering::SeqCst) {
                return None;
            }
            if let Ok(Some(count)) = self.call(|rtt| rtt.rtt_num_buffers(RttDirection::Up)) {
                if count > 0 {
                    let channels = self.describe_channels(count, config);
                    let sane = channels.iter().all(|c| c.size <= MAX_CHANNEL_SIZE);
                    if sane && channels.iter().any(|c| c.direction == "up" && c.size > 0) {
                        return Some(channels);
                    }
                }
            }
            thread::sleep(SEARCH_INTERVAL);
        }
    }

    fn describe_channels(&self, up_count: u32, config: &RttConfig) -> Vec<ChannelStatus> {
        let down_count = self
            .call(|rtt| rtt.rtt_num_buffers(RttDirection::Down))
            .ok()
            .flatten()
            .unwrap_or(0);
        let mut channels = Vec::new();
        for (direction, count) in [
            (RttDirection::Up, up_count),
            (RttDirection::Down, down_count),
        ] {
            for index in 0..count {
                let Ok(info) = self.call(move |rtt| rtt.rtt_channel(direction, index)) else {
                    continue;
                };
                let is_up = direction == RttDirection::Up;
                let configured = config.channels.get(&index);
                channels.push(ChannelStatus {
                    index,
                    name: configured
                        .filter(|_| is_up)
                        .map_or(info.name, |c| c.name.clone()),
                    direction: if is_up { "up" } else { "down" },
                    size: info.size,
                    decoder: if is_up { decoder_name(configured) } else { "" },
                    ..ChannelStatus::default()
                });
            }
        }
        channels
    }

    fn build_decoders(
        &self,
        channels: &[ChannelStatus],
        config: &RttConfig,
    ) -> (BTreeMap<u32, Box<dyn Decoder>>, Option<String>) {
        let mut decoders: BTreeMap<u32, Box<dyn Decoder>> = BTreeMap::new();
        let mut problems = Vec::new();
        for channel in channels
            .iter()
            .filter(|c| c.direction == "up" && c.size > 0)
        {
            let built = match config.channels.get(&channel.index) {
                Some(entry) => decode::build(
                    &self.binding.db,
                    self.binding.workspace.as_deref(),
                    &entry.name,
                    &entry.decode,
                ),
                None => Ok(Box::new(TextDecoder) as Box<dyn Decoder>),
            };
            match built {
                Ok(decoder) => {
                    decoders.insert(channel.index, decoder);
                }
                Err(error) => {
                    problems.push(format!("channel {}: {error}", channel.index));
                }
            }
        }
        (
            decoders,
            (!problems.is_empty()).then(|| problems.join("; ")),
        )
    }

    fn start_reader(
        &self,
        channels: Vec<u32>,
        sender: mpsc::Sender<Chunk>,
    ) -> Result<PeriodicId, String> {
        let tsdb = self.tsdb.clone();
        let mut buffer = vec![0u8; READ_CHUNK];
        self.binding
            .handle
            .periodic(Priority::P2, READ_INTERVAL, move |probe| {
                let Some(rtt) = probe.rtt() else { return };
                let t = tsdb.now_ns();
                for channel in &channels {
                    if let Ok(count) = rtt.rtt_read(*channel, &mut buffer) {
                        let _ = sender.send(Chunk {
                            channel: *channel,
                            data: buffer[..count].to_vec(),
                            t,
                        });
                    }
                }
            })
            .map_err(|e| e.to_string())
    }

    fn run(self) {
        let config = match load_config(&self.binding) {
            Ok(config) => config,
            Err(error) => {
                self.set_status("idle", None, Some(error));
                return;
            }
        };
        let block = config.control_block_address().or_else(|| {
            self.binding
                .db
                .elf_symbol("_SEGGER_RTT")
                .map(|(address, _)| address)
        });
        // Scanning RAM for a control block uses up the debug port's bandwidth,
        // so it happens only when a configuration file asks for RTT regardless.
        if block.is_none() && self.binding.rtt_config.is_none() {
            self.set_status("idle", None, None);
            return;
        }
        self.set_status("searching", block, None);
        if let Some(address) = block {
            if !self.wait_for_id(address) {
                return;
            }
        }
        if let Some(address) = block {
            // A second START with a new address is otherwise ignored by the
            // library, which keeps looking where the previous image had it.
            let command = format!("SetRTTAddr {address:#010x}");
            let result = self
                .binding
                .handle
                .call_blocking(Priority::P4, move |probe: &mut dyn Probe| {
                    probe.exec(&command)
                });
            if !matches!(result, Ok(Ok(()))) {
                tracing::warn!("setting the RTT address failed: {result:?}");
            }
        }
        if let Err(error) = self.call(move |rtt| rtt.rtt_start(block)) {
            self.set_status("idle", block, Some(error));
            return;
        }

        if let Some(channels) = self.wait_for_channels(&config) {
            self.serve(channels, &config, block);
        }
        let _ = self.call(|rtt| rtt.rtt_stop());
    }

    fn serve(&self, channels: Vec<ChannelStatus>, config: &RttConfig, block: Option<u32>) {
        let (mut decoders, problems) = self.build_decoders(&channels, config);
        *self.shared.channels.lock().expect("channel lock") = channels;
        self.set_status("running", block, problems);

        let (sender, receiver) = mpsc::channel();
        let reader = self.start_reader(decoders.keys().copied().collect(), sender);
        let reader = match reader {
            Ok(reader) => reader,
            Err(error) => {
                self.set_status("idle", block, Some(error));
                return;
            }
        };

        let mut last_stats = Instant::now();
        while !self.stop.load(Ordering::SeqCst) {
            match receiver.recv_timeout(STOP_POLL) {
                Ok(chunk) => {
                    let count = chunk.data.len() as u64;
                    if let Some(decoder) = decoders.get_mut(&chunk.channel) {
                        let mut sink = ChannelSink {
                            shared: &self.shared,
                            tsdb: &self.tsdb,
                            channel: chunk.channel,
                        };
                        decoder.feed(&chunk.data, chunk.t, &mut sink);
                    }
                    if count > 0 {
                        let mut channels = self.shared.channels.lock().expect("channel lock");
                        if let Some(status) = channels
                            .iter_mut()
                            .find(|c| c.index == chunk.channel && c.direction == "up")
                        {
                            status.bytes += count;
                        }
                    }
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }
            if last_stats.elapsed() >= Duration::from_millis(250) {
                last_stats = Instant::now();
                let mut channels = self.shared.channels.lock().expect("channel lock");
                for status in channels.iter_mut().filter(|c| c.direction == "up") {
                    if let Some(decoder) = decoders.get(&status.index) {
                        let stats = decoder.stats();
                        status.frames = stats.frames;
                        status.lost = stats.lost;
                        status.resyncs = stats.resyncs;
                    }
                }
            }
        }
        let _ = self.binding.handle.cancel(reader);
    }
}

/// Where a default config lives inside a workspace.
pub fn default_config_path(workspace: &std::path::Path) -> Option<PathBuf> {
    let path = workspace.join(".jlinkd").join("rtt.yaml");
    path.exists().then_some(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_history_is_bounded_and_cursor_based() {
        let mut terminal = Terminal::default();
        terminal.push(&vec![1u8; TERMINAL_HISTORY]);
        terminal.push(&[2, 3]);
        assert_eq!(terminal.bytes.len(), TERMINAL_HISTORY);
        let (all, next) = terminal.read_since(0);
        assert_eq!(all.len(), TERMINAL_HISTORY);
        assert_eq!(next, TERMINAL_HISTORY as u64 + 2);
        terminal.push(&[4]);
        assert_eq!(terminal.read_since(next).0, [4]);
    }
}
