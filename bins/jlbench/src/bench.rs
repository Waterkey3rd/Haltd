//! The individual measurements. Everything takes a `dyn Probe`, so the logic is
//! tested against the mock; only the numbers need real hardware.

use std::path::PathBuf;
use std::thread::sleep;
use std::time::{Duration, Instant};

use probe_api::{
    enable_cycle_counter, AccessWidth, BpKind, Histogram, HistogramSummary, HssBlock, HssCaps,
    Probe, ProbeError, ProbeExt, Reg, ResetKind, Result, RttDirection, HSS_FLAG_TIMESTAMP_US,
};
use serde::Serialize;
use symbols::{LoadOptions, SymbolDb};

pub const DWT_CYCCNT: u32 = 0xE000_1004;

const READ_SIZES: [usize; 4] = [4, 64, 1024, 16 * 1024];
const LADDER_BLOCK: usize = 4096;
const LADDER_ROUNDS: usize = 100;
const HSS_DETECT_TIME: Duration = Duration::from_millis(400);
const HSS_READ_INTERVAL: Duration = Duration::from_millis(10);
const HSS_MEM_PROBE_INTERVAL: Duration = Duration::from_millis(100);
const HALT_TIMEOUT: Duration = Duration::from_secs(1);
const RTT_FIND_TIMEOUT: Duration = Duration::from_secs(2);
const FLASH_BP_ROUNDS: usize = 2;

#[derive(Clone, Debug)]
pub struct BenchOptions {
    pub speeds: Vec<u32>,
    pub base_speed: u32,
    pub flash_addr: u32,
    pub ram_addr: u32,
    pub iterations: usize,
    /// Permit tests that stop the core.
    pub allow_halt: bool,
    /// Permit tests that write flash.
    pub allow_flash: bool,
    pub hss_period_us: u32,
    pub hss_time: Duration,
    pub rtt_time: Duration,
    pub rtt_control_block: Option<u32>,
    pub elf: Option<PathBuf>,
    pub elf_alt: Option<PathBuf>,
    /// Where to cycle the flash breakpoint; must be entered repeatedly for the
    /// hit and step-off timings. Defaults to the function holding the PC.
    pub flash_bp_function: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(tag = "status", content = "result", rename_all = "lowercase")]
pub enum Section<T> {
    Done(T),
    Skipped(String),
    Failed(String),
}

impl<T> Section<T> {
    fn from_result(result: Result<T>) -> Self {
        match result {
            Ok(value) => Section::Done(value),
            Err(ProbeError::Unsupported(what)) => Section::Skipped(format!("unsupported: {what}")),
            Err(error) => Section::Failed(error.to_string()),
        }
    }
}

#[derive(Debug, Serialize)]
pub struct SpeedResult {
    pub requested_khz: u32,
    pub actual_khz: u32,
    pub rounds: usize,
    pub errors: usize,
    pub kilobytes_per_second: f64,
    pub first_error: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct LatencyResult {
    pub operation: String,
    pub latency: HistogramSummary,
}

#[derive(Debug, Serialize)]
pub struct RunControlResult {
    pub halt: HistogramSummary,
    pub go: HistogramSummary,
    pub step: HistogramSummary,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct RecordFormat {
    pub header_bytes: usize,
    pub record_bytes: usize,
    /// Median advance of the counter per record.
    pub counter_delta: u32,
}

#[derive(Debug, Serialize)]
pub struct HssFormatProbe {
    pub flags: u32,
    pub captured_bytes: usize,
    pub format: Option<RecordFormat>,
    /// Second block held the constant it should, confirming block order.
    pub second_block_matches: Option<bool>,
    /// Median advance of the header word, when there is a header.
    pub header_delta: Option<u32>,
    pub first_bytes_hex: String,
}

#[derive(Debug, Serialize)]
pub struct HssResult {
    pub caps: HssCaps,
    pub formats: Vec<HssFormatProbe>,
    pub sustained_blocks: u32,
    pub period_us: u32,
    pub seconds: f64,
    pub records: u64,
    pub lost_records: Option<u64>,
    pub bytes_per_second: f64,
    pub memory_reads_ok: u64,
    pub memory_reads_failed: u64,
    pub memory_read_latency: HistogramSummary,
}

#[derive(Debug, Serialize)]
pub struct RttResult {
    pub up_channels: u32,
    pub seconds: f64,
    pub bytes: u64,
    pub bytes_per_second: f64,
    pub host_overflows: i32,
}

#[derive(Debug, Serialize)]
pub struct FlashResult {
    pub unchanged_ms: f64,
    pub changed_ms: Option<f64>,
    pub restore_ms: Option<f64>,
}

#[derive(Debug, Serialize)]
pub struct FlashBpResult {
    pub address: u32,
    pub function: String,
    pub rounds: usize,
    pub hits: usize,
    pub set_bp: HistogramSummary,
    pub go_with_bp: HistogramSummary,
    pub step_off_bp: HistogramSummary,
    pub clear_bp: HistogramSummary,
    pub go_after_clear: HistogramSummary,
}

/// Measure each interface speed in ascending order, stopping after the first
/// unreliable one, then restore the base speed.
pub fn speed_ladder(probe: &mut dyn Probe, options: &BenchOptions) -> Result<Vec<SpeedResult>> {
    let mut results = Vec::new();
    for &requested in &options.speeds {
        let actual = probe.set_speed(requested)?;
        let result = measure_speed(probe, options.flash_addr, requested, actual);
        let clean = result.errors == 0;
        results.push(result);
        if !clean {
            break;
        }
    }
    probe.set_speed(options.base_speed)?;
    Ok(results)
}

fn measure_speed(probe: &mut dyn Probe, address: u32, requested: u32, actual: u32) -> SpeedResult {
    let mut reference = vec![0u8; LADDER_BLOCK];
    let mut buffer = vec![0u8; LADDER_BLOCK];
    let (mut errors, mut first_error) = (0, None);
    if let Err(error) = probe.read_mem(address, &mut reference, AccessWidth::Auto) {
        errors += 1;
        first_error = Some(error.to_string());
    }
    let started = Instant::now();
    for _ in 0..LADDER_ROUNDS {
        match probe.read_mem(address, &mut buffer, AccessWidth::Auto) {
            Ok(()) if buffer == reference => {}
            Ok(()) => {
                errors += 1;
                first_error.get_or_insert_with(|| "data mismatch".to_owned());
            }
            Err(error) => {
                errors += 1;
                first_error.get_or_insert_with(|| error.to_string());
            }
        }
    }
    let seconds = started.elapsed().as_secs_f64();
    SpeedResult {
        requested_khz: requested,
        actual_khz: actual,
        rounds: LADDER_ROUNDS,
        errors,
        kilobytes_per_second: (LADDER_BLOCK * LADDER_ROUNDS) as f64 / 1000.0 / seconds,
        first_error,
    }
}

/// The highest speed with no errors.
pub fn stable_speed(results: &[SpeedResult]) -> Option<u32> {
    results
        .iter()
        .filter(|r| r.errors == 0)
        .map(|r| r.actual_khz)
        .max()
}

pub fn latency(probe: &mut dyn Probe, options: &BenchOptions) -> Result<Vec<LatencyResult>> {
    let mut results = Vec::new();
    for size in READ_SIZES {
        let mut buffer = vec![0u8; size];
        let mut histogram = Histogram::default();
        for _ in 0..options.iterations {
            let started = Instant::now();
            probe.read_mem(options.ram_addr, &mut buffer, AccessWidth::Auto)?;
            histogram.record(started.elapsed());
        }
        results.push(LatencyResult {
            operation: format!("ReadMemEx {size} B"),
            latency: histogram.summary(),
        });
    }
    let mut histogram = Histogram::default();
    for _ in 0..options.iterations {
        let started = Instant::now();
        probe.is_halted()?;
        histogram.record(started.elapsed());
    }
    results.push(LatencyResult {
        operation: "IsHalted".to_owned(),
        latency: histogram.summary(),
    });
    Ok(results)
}

fn wait_for_state(probe: &mut dyn Probe, halted: bool) -> Result<bool> {
    let started = Instant::now();
    while started.elapsed() < HALT_TIMEOUT {
        if probe.is_halted()? == halted {
            return Ok(true);
        }
    }
    Ok(false)
}

pub fn run_control(probe: &mut dyn Probe, options: &BenchOptions) -> Result<RunControlResult> {
    let was_halted = probe.is_halted()?;
    let (mut halt, mut go) = (Histogram::default(), Histogram::default());
    let mut step = Histogram::default();
    let outcome = (|| -> Result<()> {
        let rounds = options.iterations.min(100);
        if was_halted {
            probe.go()?;
            wait_for_state(probe, false)?;
        }
        for _ in 0..rounds {
            let started = Instant::now();
            probe.halt()?;
            wait_for_state(probe, true)?;
            halt.record(started.elapsed());

            let started = Instant::now();
            probe.go()?;
            wait_for_state(probe, false)?;
            go.record(started.elapsed());
        }
        probe.halt()?;
        wait_for_state(probe, true)?;
        for _ in 0..rounds {
            let started = Instant::now();
            probe.step_instr()?;
            step.record(started.elapsed());
        }
        Ok(())
    })();
    // Leave the core as we found it, even after a failure.
    let restored = if was_halted { probe.halt() } else { probe.go() };
    outcome?;
    restored?;
    Ok(RunControlResult {
        halt: halt.summary(),
        go: go.summary(),
        step: step.summary(),
    })
}

fn read_word(data: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes([
        data[offset],
        data[offset + 1],
        data[offset + 2],
        data[offset + 3],
    ])
}

/// Median step of a little-endian counter sampled every `record` bytes at
/// `offset`, if the steps are regular enough to be that counter.
fn regular_delta(data: &[u8], record: usize, offset: usize) -> Option<u32> {
    let count = data.len() / record;
    if count < 8 {
        return None;
    }
    let mut deltas: Vec<u32> = (1..count)
        .map(|i| {
            read_word(data, i * record + offset)
                .wrapping_sub(read_word(data, (i - 1) * record + offset))
        })
        .collect();
    deltas.sort_unstable();
    let median = deltas[deltas.len() / 2];
    if median == 0 {
        return None;
    }
    let tolerance = median / 4;
    let close = deltas
        .iter()
        .filter(|d| d.abs_diff(median) <= tolerance)
        .count();
    (close * 5 >= deltas.len() * 4).then_some(median)
}

/// Work out the record layout from a capture whose first block is a counter.
/// Tries the shortest headers first, so the simplest explanation wins.
pub fn detect_record_format(data: &[u8], payload_bytes: usize) -> Option<RecordFormat> {
    [0, 4, 8, 12, 16].into_iter().find_map(|header_bytes| {
        let record_bytes = header_bytes + payload_bytes;
        if !data.len().is_multiple_of(record_bytes) {
            return None;
        }
        regular_delta(data, record_bytes, header_bytes).map(|counter_delta| RecordFormat {
            header_bytes,
            record_bytes,
            counter_delta,
        })
    })
}

/// Counts records missing from a stream, from jumps in the leading counter.
#[derive(Debug)]
pub struct LossCounter {
    format: RecordFormat,
    previous: Option<u32>,
    pub records: u64,
    pub lost: u64,
}

impl LossCounter {
    pub fn new(format: RecordFormat) -> Self {
        Self {
            format,
            previous: None,
            records: 0,
            lost: 0,
        }
    }

    pub fn feed(&mut self, data: &[u8]) {
        for record in data.chunks_exact(self.format.record_bytes) {
            let counter = read_word(record, self.format.header_bytes);
            if let Some(previous) = self.previous {
                let delta = counter.wrapping_sub(previous);
                let periods = (f64::from(delta) / f64::from(self.format.counter_delta)).round();
                if periods > 1.0 {
                    self.lost += periods as u64 - 1;
                }
            }
            self.previous = Some(counter);
            self.records += 1;
        }
    }
}

fn capture_hss(
    probe: &mut dyn Probe,
    blocks: &[HssBlock],
    period_us: u32,
    flags: u32,
    time: Duration,
) -> Result<Vec<u8>> {
    let hss = probe
        .hss()
        .ok_or_else(|| ProbeError::Unsupported("HSS".to_owned()))?;
    hss.hss_start(blocks, period_us, flags)?;
    let mut captured = Vec::new();
    let mut buffer = vec![0u8; 64 * 1024];
    let started = Instant::now();
    let result = loop {
        match hss.hss_read(&mut buffer) {
            Ok(count) => captured.extend_from_slice(&buffer[..count]),
            Err(error) => break Err(error),
        }
        if started.elapsed() >= time {
            break Ok(());
        }
        sleep(HSS_READ_INTERVAL);
    };
    let stopped = hss.hss_stop();
    result?;
    stopped?;
    Ok(captured)
}

fn probe_format(
    probe: &mut dyn Probe,
    options: &BenchOptions,
    flags: u32,
    constant: u32,
) -> Result<HssFormatProbe> {
    let blocks = [
        HssBlock {
            addr: DWT_CYCCNT,
            num_bytes: 4,
        },
        HssBlock {
            addr: options.flash_addr,
            num_bytes: 4,
        },
    ];
    let data = capture_hss(
        probe,
        &blocks,
        options.hss_period_us,
        flags,
        HSS_DETECT_TIME,
    )?;
    let format = detect_record_format(&data, 8);
    let second_block_matches = format.map(|f| {
        data.chunks_exact(f.record_bytes)
            .all(|record| read_word(record, f.header_bytes + 4) == constant)
    });
    let header_delta = format
        .filter(|f| f.header_bytes >= 4)
        .and_then(|f| regular_delta(&data, f.record_bytes, 0));
    Ok(HssFormatProbe {
        flags,
        captured_bytes: data.len(),
        format,
        second_block_matches,
        header_delta,
        first_bytes_hex: data
            .iter()
            .take(64)
            .map(|b| format!("{b:02x}"))
            .collect::<String>(),
    })
}

pub fn hss(probe: &mut dyn Probe, options: &BenchOptions) -> Result<HssResult> {
    enable_cycle_counter(probe)?;
    let constant = probe.read_u32(options.flash_addr)?;
    let caps = probe
        .hss()
        .ok_or_else(|| ProbeError::Unsupported("HSS".to_owned()))?
        .hss_caps()?;

    let plain = probe_format(probe, options, 0, constant)?;
    let stamped = probe_format(probe, options, HSS_FLAG_TIMESTAMP_US, constant)?;

    let block_count = caps.max_blocks.clamp(1, 16);
    let mut blocks = vec![HssBlock {
        addr: DWT_CYCCNT,
        num_bytes: 4,
    }];
    blocks.extend((1..block_count).map(|i| HssBlock {
        addr: options.ram_addr + i * 4,
        num_bytes: 4,
    }));

    // The format for this block count, measured with the same flags.
    let payload = blocks.len() * 4;
    let plain_header = plain.format.map(|f| f.header_bytes);
    let period_cycles = plain.format.map(|f| f.counter_delta);
    let mut counter = match (plain_header, period_cycles) {
        (Some(header_bytes), Some(counter_delta)) => Some(LossCounter::new(RecordFormat {
            header_bytes,
            record_bytes: header_bytes + payload,
            counter_delta,
        })),
        _ => None,
    };

    let mut memory_latency = Histogram::default();
    let (mut memory_ok, mut memory_failed, mut bytes) = (0u64, 0u64, 0u64);
    let hss_api = |p: &mut dyn Probe| -> Result<()> {
        p.hss()
            .ok_or_else(|| ProbeError::Unsupported("HSS".to_owned()))?
            .hss_start(&blocks, options.hss_period_us, 0)
    };
    hss_api(probe)?;

    let mut buffer = vec![0u8; 64 * 1024];
    let mut pending: Vec<u8> = Vec::new();
    let started = Instant::now();
    let mut next_memory_probe = started;
    let outcome = loop {
        let read = match probe.hss() {
            Some(api) => api.hss_read(&mut buffer),
            None => break Err(ProbeError::Unsupported("HSS".to_owned())),
        };
        match read {
            Ok(count) => {
                bytes += count as u64;
                if let Some(counter) = counter.as_mut() {
                    pending.extend_from_slice(&buffer[..count]);
                    let whole = pending.len() - pending.len() % counter.format.record_bytes;
                    counter.feed(&pending[..whole]);
                    pending.drain(..whole);
                }
            }
            Err(error) => break Err(error),
        }
        if Instant::now() >= next_memory_probe {
            next_memory_probe += HSS_MEM_PROBE_INTERVAL;
            let mut word = [0u8; 4];
            let probe_started = Instant::now();
            match probe.read_mem(options.ram_addr, &mut word, AccessWidth::Auto) {
                Ok(()) => {
                    memory_ok += 1;
                    memory_latency.record(probe_started.elapsed());
                }
                Err(_) => memory_failed += 1,
            }
        }
        if started.elapsed() >= options.hss_time {
            break Ok(());
        }
        sleep(HSS_READ_INTERVAL);
    };
    let seconds = started.elapsed().as_secs_f64();
    let stopped = probe.hss().map_or(Ok(()), |api| api.hss_stop());
    outcome?;
    stopped?;

    Ok(HssResult {
        caps,
        formats: vec![plain, stamped],
        sustained_blocks: block_count,
        period_us: options.hss_period_us,
        seconds,
        records: counter.as_ref().map_or(0, |c| c.records),
        lost_records: counter.as_ref().map(|c| c.lost),
        bytes_per_second: bytes as f64 / seconds,
        memory_reads_ok: memory_ok,
        memory_reads_failed: memory_failed,
        memory_read_latency: memory_latency.summary(),
    })
}

fn rtt_control_block(options: &BenchOptions) -> Option<u32> {
    options.rtt_control_block.or_else(|| {
        let elf = options.elf.as_ref()?;
        let db = SymbolDb::load(elf, &LoadOptions::default()).ok()?;
        db.elf_symbol("_SEGGER_RTT").map(|(address, _)| address)
    })
}

pub fn rtt(probe: &mut dyn Probe, options: &BenchOptions) -> Section<RttResult> {
    let block = rtt_control_block(options);
    if block.is_none() && options.elf.is_some() {
        return Section::Skipped("the ELF has no _SEGGER_RTT symbol".to_owned());
    }
    Section::from_result(rtt_inner(probe, options, block)).or_skip_if_absent()
}

impl Section<Option<RttResult>> {
    fn or_skip_if_absent(self) -> Section<RttResult> {
        match self {
            Section::Done(Some(result)) => Section::Done(result),
            Section::Done(None) => {
                Section::Skipped("no RTT control block found on the target".to_owned())
            }
            Section::Skipped(reason) => Section::Skipped(reason),
            Section::Failed(reason) => Section::Failed(reason),
        }
    }
}

fn rtt_inner(
    probe: &mut dyn Probe,
    options: &BenchOptions,
    block: Option<u32>,
) -> Result<Option<RttResult>> {
    let api = probe
        .rtt()
        .ok_or_else(|| ProbeError::Unsupported("RTT".to_owned()))?;
    api.rtt_start(block)?;
    let found = Instant::now();
    let channels = loop {
        if let Some(count) = api.rtt_num_buffers(RttDirection::Up)? {
            break count;
        }
        if found.elapsed() > RTT_FIND_TIMEOUT {
            api.rtt_stop()?;
            return Ok(None);
        }
        sleep(HSS_READ_INTERVAL);
    };
    let mut buffer = vec![0u8; 64 * 1024];
    let mut bytes = 0u64;
    let started = Instant::now();
    while started.elapsed() < options.rtt_time {
        for channel in 0..channels {
            bytes += api.rtt_read(channel, &mut buffer)? as u64;
        }
        sleep(Duration::from_millis(5));
    }
    let seconds = started.elapsed().as_secs_f64();
    let status = api.rtt_status()?;
    api.rtt_stop()?;
    Ok(Some(RttResult {
        up_channels: channels,
        seconds,
        bytes,
        bytes_per_second: bytes as f64 / seconds,
        host_overflows: status.host_overflows,
    }))
}

fn timed_download(probe: &mut dyn Probe, file: &std::path::Path) -> Result<f64> {
    let started = Instant::now();
    probe.download(file, &|_| {})?;
    Ok(started.elapsed().as_secs_f64() * 1000.0)
}

pub fn flash(probe: &mut dyn Probe, options: &BenchOptions) -> Section<FlashResult> {
    if !options.allow_flash {
        return Section::Skipped("needs --allow-flash".to_owned());
    }
    let Some(elf) = options.elf.as_deref() else {
        return Section::Skipped("needs --elf".to_owned());
    };
    let measured = (|| -> Result<FlashResult> {
        // Programming twice makes the second pass an all-sectors-match run.
        timed_download(probe, elf)?;
        let unchanged_ms = timed_download(probe, elf)?;
        let (mut changed_ms, mut restore_ms) = (None, None);
        if let Some(alternative) = options.elf_alt.as_deref() {
            changed_ms = Some(timed_download(probe, alternative)?);
            restore_ms = Some(timed_download(probe, elf)?);
        }
        Ok(FlashResult {
            unchanged_ms,
            changed_ms,
            restore_ms,
        })
    })();
    let reset = probe.reset(ResetKind::Normal, false);
    Section::from_result(measured.and_then(|result| reset.map(|()| result)))
}

/// Cycle a flash breakpoint at the start of the function the core is in.
pub fn flash_breakpoint(probe: &mut dyn Probe, options: &BenchOptions) -> Section<FlashBpResult> {
    if !options.allow_flash || !options.allow_halt {
        return Section::Skipped("needs --allow-flash and --allow-halt".to_owned());
    }
    let Some(elf) = options.elf.as_deref() else {
        return Section::Skipped("needs --elf".to_owned());
    };
    let db = match SymbolDb::load(elf, &LoadOptions::default()) {
        Ok(db) => db,
        Err(error) => return Section::Failed(error.to_string()),
    };
    let outcome = flash_bp_rounds(probe, &db, options.flash_bp_function.as_deref());
    let resumed = probe.go();
    Section::from_result(outcome.and_then(|result| resumed.map(|()| result))).flatten_skip()
}

impl<T> Section<std::result::Result<T, String>> {
    fn flatten_skip(self) -> Section<T> {
        match self {
            Section::Done(Ok(value)) => Section::Done(value),
            Section::Done(Err(reason)) | Section::Skipped(reason) => Section::Skipped(reason),
            Section::Failed(reason) => Section::Failed(reason),
        }
    }
}

fn flash_bp_rounds(
    probe: &mut dyn Probe,
    db: &SymbolDb,
    function_name: Option<&str>,
) -> Result<std::result::Result<FlashBpResult, String>> {
    let timed = |histogram: &mut Histogram, action: &mut dyn FnMut() -> Result<()>| -> Result<()> {
        let started = Instant::now();
        action()?;
        histogram.record(started.elapsed());
        Ok(())
    };
    probe.halt()?;
    wait_for_state(probe, true)?;
    let mut pc = [0u32];
    probe.read_regs(&[Reg::PC], &mut pc)?;
    let chosen = match function_name {
        Some(name) => db.functions(name).into_iter().next(),
        None => db.function_at(pc[0]),
    };
    let Some(function) = chosen else {
        return Ok(Err(match function_name {
            Some(name) => format!("no function named {name}"),
            None => format!("no function contains PC {:#010x}", pc[0]),
        }));
    };
    let address = function.low;

    let mut set_bp = Histogram::default();
    let mut go_with_bp = Histogram::default();
    let mut step_off_bp = Histogram::default();
    let mut clear_bp = Histogram::default();
    let mut go_after_clear = Histogram::default();
    let mut hits = 0;

    for _ in 0..FLASH_BP_ROUNDS {
        let mut handle = None;
        timed(&mut set_bp, &mut || {
            handle = Some(probe.set_bp(address, BpKind::Flash)?);
            Ok(())
        })?;
        timed(&mut go_with_bp, &mut || probe.go())?;
        if wait_for_state(probe, true)? {
            hits += 1;
            timed(&mut step_off_bp, &mut || probe.step_instr())?;
        } else {
            probe.halt()?;
            wait_for_state(probe, true)?;
        }
        if let Some(handle) = handle {
            timed(&mut clear_bp, &mut || probe.clear_bp(handle))?;
        }
        timed(&mut go_after_clear, &mut || probe.go())?;
        sleep(Duration::from_millis(50));
        probe.halt()?;
        wait_for_state(probe, true)?;
    }
    Ok(Ok(FlashBpResult {
        address,
        function: function.name,
        rounds: FLASH_BP_ROUNDS,
        hits,
        set_bp: set_bp.summary(),
        go_with_bp: go_with_bp.summary(),
        step_off_bp: step_off_bp.summary(),
        clear_bp: clear_bp.summary(),
        go_after_clear: go_after_clear.summary(),
    }))
}

pub fn section_of<T>(result: Result<T>) -> Section<T> {
    Section::from_result(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use probe_mock::MockProbe;

    const RAM: u32 = 0x2000_0000;
    const FLASH: u32 = 0x0800_0000;

    fn options() -> BenchOptions {
        BenchOptions {
            speeds: vec![4000, 6000],
            base_speed: 4000,
            flash_addr: FLASH,
            ram_addr: RAM,
            iterations: 5,
            allow_halt: false,
            allow_flash: false,
            hss_period_us: 1000,
            hss_time: Duration::from_millis(30),
            rtt_time: Duration::from_millis(10),
            rtt_control_block: None,
            elf: None,
            elf_alt: None,
            flash_bp_function: None,
        }
    }

    fn target() -> MockProbe {
        let mut mock = MockProbe::new();
        mock.add_region(RAM, vec![0; 0x8000]);
        mock.add_region(FLASH, (0..0x4000u32).map(|i| i as u8).collect());
        mock.add_region(0xE000_0000, vec![0; 0x10000]);
        mock
    }

    fn stream(header: usize, count: u32, step: u32, skip: Option<u32>) -> Vec<u8> {
        let mut data = Vec::new();
        let mut counter = 1000u32;
        for i in 0..count {
            if Some(i) == skip {
                counter = counter.wrapping_add(step * 3);
            }
            data.extend_from_slice(&vec![0xEEu8; header]);
            data.extend_from_slice(&counter.to_le_bytes());
            data.extend_from_slice(&[7u8; 4]);
            counter = counter.wrapping_add(step);
        }
        data
    }

    #[test]
    fn format_detection_finds_each_header_size() {
        for header in [0usize, 4, 8] {
            let found = detect_record_format(&stream(header, 50, 72_000, None), 8).unwrap();
            assert_eq!(found.header_bytes, header);
            assert_eq!(found.record_bytes, header + 8);
            assert_eq!(found.counter_delta, 72_000);
        }
    }

    #[test]
    fn format_detection_rejects_constant_and_short_data() {
        assert!(detect_record_format(&[5u8; 400], 8).is_none());
        assert!(detect_record_format(&stream(0, 3, 72_000, None), 8).is_none());
    }

    #[test]
    fn counter_wraparound_does_not_break_detection() {
        let mut data = Vec::new();
        let mut counter = u32::MAX - 100_000;
        for _ in 0..40 {
            data.extend_from_slice(&counter.to_le_bytes());
            data.extend_from_slice(&[0u8; 4]);
            counter = counter.wrapping_add(72_000);
        }
        let format = detect_record_format(&data, 8).unwrap();
        let mut loss = LossCounter::new(format);
        loss.feed(&data);
        assert_eq!((loss.records, loss.lost), (40, 0));
    }

    #[test]
    fn loss_counter_measures_gaps_across_calls() {
        let data = stream(0, 20, 1000, Some(10));
        let format = RecordFormat {
            header_bytes: 0,
            record_bytes: 8,
            counter_delta: 1000,
        };
        let mut loss = LossCounter::new(format);
        let (head, tail) = data.split_at(64);
        loss.feed(head);
        loss.feed(tail);
        assert_eq!(loss.records, 20);
        assert_eq!(loss.lost, 3);
    }

    #[test]
    fn stable_speed_ignores_failing_rates() {
        let result = |actual_khz, errors| SpeedResult {
            requested_khz: actual_khz,
            actual_khz,
            rounds: 1,
            errors,
            kilobytes_per_second: 1.0,
            first_error: None,
        };
        assert_eq!(
            stable_speed(&[result(4000, 0), result(6000, 0), result(12000, 2)]),
            Some(6000)
        );
        assert_eq!(stable_speed(&[result(4000, 1)]), None);
    }

    #[test]
    fn ladder_reads_cleanly_on_the_mock() {
        let mut mock = target();
        let results = speed_ladder(&mut mock, &options()).unwrap();
        assert_eq!(results.len(), 2);
        assert!(results.iter().all(|r| r.errors == 0));
    }

    #[test]
    fn hss_section_reports_format_and_no_loss() {
        let mut mock = target();
        mock.set_hss_auto(10, DWT_CYCCNT, 72_000);
        let result = hss(&mut mock, &options()).unwrap();

        let plain = result.formats[0].format.unwrap();
        assert_eq!(plain.header_bytes, 4);
        assert_eq!(result.formats[0].second_block_matches, Some(true));
        assert_eq!(result.formats[0].header_delta, Some(1));

        let stamped = result.formats[1].format.unwrap();
        assert_eq!(stamped.header_bytes, 4);
        assert_eq!(result.formats[1].header_delta, Some(1000));

        assert_eq!(result.sustained_blocks, 8);
        assert!(result.records > 0);
        assert_eq!(result.lost_records, Some(0));
        assert!(result.memory_reads_ok > 0);
    }

    #[test]
    fn run_control_restores_a_running_core() {
        let mut mock = target();
        let result = run_control(&mut mock, &options()).unwrap();
        assert_eq!(result.halt.count, 5);
        assert_eq!(result.step.count, 5);
        assert!(!mock.is_halted().unwrap());
    }

    #[test]
    fn intrusive_sections_need_explicit_permission() {
        let mut mock = target();
        assert!(matches!(flash(&mut mock, &options()), Section::Skipped(_)));
        assert!(matches!(
            flash_breakpoint(&mut mock, &options()),
            Section::Skipped(_)
        ));
    }

    #[test]
    fn rtt_without_a_control_block_on_a_mock_is_measured() {
        let mut mock = target();
        mock.push_rtt_up(0, &[1; 100]);
        let mut opts = options();
        opts.rtt_control_block = Some(RAM);
        let Section::Done(result) = rtt(&mut mock, &opts) else {
            panic!("expected a result");
        };
        assert_eq!(result.bytes, 100);
    }
}
