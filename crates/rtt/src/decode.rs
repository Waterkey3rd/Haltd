//! Turning channel bytes into terminal text and named signals (DESIGN §5.8).

use std::borrow::Cow;
use std::collections::HashSet;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::Path;
use std::sync::OnceLock;

use mlua::{Function, Lua, Table, Value};
use regex::Regex;
use sampler::codec::{leaves_of_place, Scalar};
use sampler::timesync::ProbeClock;
use symbols::{MemoryReader, Place, SymbolDb, TypeKind, TypeRef};
use visualizer::Visualizers;

use crate::config::{Decode, StructConfig, TimelineEntry, Timestamp};

pub trait Sink {
    /// `plot` asks for a Timeline series; the latest value is kept regardless.
    fn sample(&mut self, name: &str, group: Option<&str>, plot: bool, t: i64, value: f64);
    fn text(&mut self, data: &[u8]);
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DecoderStats {
    pub frames: u64,
    pub lost: u64,
    pub resyncs: u64,
}

pub trait Decoder: Send {
    /// `data` is empty when a read found nothing, which timing logic uses.
    fn feed(&mut self, data: &[u8], t: i64, sink: &mut dyn Sink);

    fn stats(&self) -> DecoderStats {
        DecoderStats::default()
    }
}

pub struct TextDecoder;

impl Decoder for TextDecoder {
    fn feed(&mut self, data: &[u8], _t: i64, sink: &mut dyn Sink) {
        if !data.is_empty() {
            sink.text(data);
        }
    }
}

/// Numeric named captures of each line become signals. Lines still reach the
/// terminal.
pub struct RegexDecoder {
    channel: String,
    regex: Regex,
    group: Option<String>,
    names: Vec<String>,
    line: Vec<u8>,
    matched: u64,
}

impl RegexDecoder {
    pub fn new(channel: &str, pattern: &str, group: Option<String>) -> Result<Self, String> {
        let regex = Regex::new(pattern).map_err(|error| error.to_string())?;
        let names: Vec<String> = regex.capture_names().flatten().map(str::to_owned).collect();
        if names.is_empty() {
            return Err("the pattern has no named capture groups".to_owned());
        }
        Ok(Self {
            channel: channel.to_owned(),
            regex,
            group,
            names,
            line: Vec::new(),
            matched: 0,
        })
    }

    fn finish_line(&mut self, t: i64, sink: &mut dyn Sink) {
        let line = String::from_utf8_lossy(&self.line).into_owned();
        self.line.clear();
        let Some(captures) = self.regex.captures(&line) else {
            return;
        };
        self.matched += 1;
        for name in &self.names {
            let value = captures
                .name(name)
                .and_then(|m| m.as_str().parse::<f64>().ok());
            if let Some(value) = value {
                let signal = format!("{}.{name}", self.channel);
                sink.sample(&signal, self.group.as_deref(), true, t, value);
            }
        }
    }
}

impl Decoder for RegexDecoder {
    fn feed(&mut self, data: &[u8], t: i64, sink: &mut dyn Sink) {
        if data.is_empty() {
            return;
        }
        sink.text(data);
        for byte in data {
            if *byte == b'\n' {
                self.finish_line(t, sink);
            } else if *byte != b'\r' {
                self.line.push(*byte);
            }
        }
    }

    fn stats(&self) -> DecoderStats {
        DecoderStats {
            frames: self.matched,
            ..DecoderStats::default()
        }
    }
}

const MAX_KV_SIGNALS: usize = 32;
const MAX_KV_LINE: usize = 4096;

fn kv_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"(?:^|[^A-Za-z0-9_.])([A-Za-z_][A-Za-z0-9_.\[\]]*)\s*[=:]\s*(-?\d+(?:\.\d+)?(?:[eE][-+]?\d+)?)",
        )
        .expect("valid key=value regex")
    })
}

fn ansi_csi_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\x1b\[[0-9;?]*[A-Za-z]").expect("valid ANSI CSI regex"))
}

fn strip_ansi(line: &str) -> Cow<'_, str> {
    if line.contains('\x1b') {
        ansi_csi_regex().replace_all(line, "")
    } else {
        Cow::Borrowed(line)
    }
}

/// Automatic text decoder that forwards raw bytes to the terminal while
/// extracting `name=value` / `name: value` numbers into signals.
pub struct KeyValueDecoder {
    channel: String,
    line: Vec<u8>,
    seen: HashSet<String>,
    warned_cap: bool,
    matched: u64,
}

impl KeyValueDecoder {
    pub fn new(channel: &str) -> Self {
        Self {
            channel: channel.to_owned(),
            line: Vec::new(),
            seen: HashSet::new(),
            warned_cap: false,
            matched: 0,
        }
    }

    fn finish_line(&mut self, t: i64, sink: &mut dyn Sink) {
        if self.line.is_empty() {
            return;
        }
        let raw = String::from_utf8_lossy(&self.line).into_owned();
        self.line.clear();
        let clean = strip_ansi(&raw);
        let mut line_matched = false;
        for captures in kv_regex().captures_iter(&clean) {
            let Some(name_match) = captures.get(1) else {
                continue;
            };
            let Some(num_match) = captures.get(2) else {
                continue;
            };
            let tail = &clean[num_match.end()..];
            // Skip version chains like 1.2.3 and hex/binary literals like 0x2000.
            if tail.starts_with('.') {
                continue;
            }
            if matches!(num_match.as_str(), "0" | "-0")
                && tail
                    .chars()
                    .next()
                    .is_some_and(|c| matches!(c, 'x' | 'X' | 'b' | 'B'))
            {
                continue;
            }
            let Ok(value) = num_match.as_str().parse::<f64>() else {
                continue;
            };
            if !value.is_finite() {
                continue;
            }
            let signal = format!("{}.{}", self.channel, name_match.as_str());
            if !self.seen.contains(&signal) {
                if self.seen.len() >= MAX_KV_SIGNALS {
                    if !self.warned_cap {
                        self.warned_cap = true;
                        tracing::warn!(
                            "RTT channel {}: exceeded {MAX_KV_SIGNALS} auto-decoded signals; ignoring further names",
                            self.channel
                        );
                    }
                    continue;
                }
                self.seen.insert(signal.clone());
            }
            line_matched = true;
            sink.sample(&signal, None, true, t, value);
        }
        if line_matched {
            self.matched += 1;
        }
    }
}

impl Decoder for KeyValueDecoder {
    fn feed(&mut self, data: &[u8], t: i64, sink: &mut dyn Sink) {
        if data.is_empty() {
            return;
        }
        sink.text(data);
        for &byte in data {
            if byte == b'\n' {
                self.finish_line(t, sink);
            } else if byte != b'\r' {
                if self.line.len() >= MAX_KV_LINE {
                    self.line.clear();
                }
                self.line.push(byte);
            }
        }
    }

    fn stats(&self) -> DecoderStats {
        DecoderStats {
            frames: self.matched,
            ..DecoderStats::default()
        }
    }
}

/// Field of a binary frame.
#[derive(Clone, Debug)]
pub struct FrameField {
    pub name: String,
    pub offset: usize,
    pub scalar: Scalar,
    pub plot: bool,
    pub group: Option<String>,
}

/// Where the framing fields are.
#[derive(Clone, Debug)]
pub struct FrameLayout {
    pub size: usize,
    pub sync: Vec<u8>,
    pub sync_offset: usize,
    pub sequence: Option<(usize, Scalar)>,
    pub timestamp: Option<(usize, Scalar, f64)>,
    pub fields: Vec<FrameField>,
}

/// Binary frames laid out by a type from the ELF (DESIGN §5.8).
pub struct StructDecoder {
    layout: FrameLayout,
    buffer: Vec<u8>,
    locked: bool,
    last_sequence: Option<u64>,
    clock: Option<ProbeClock>,
    stats: DecoderStats,
}

impl StructDecoder {
    pub fn new(layout: FrameLayout) -> Self {
        let clock = layout
            .timestamp
            .as_ref()
            .map(|(_, _, ns_per_tick)| ProbeClock::for_ticks(*ns_per_tick));
        Self {
            layout,
            buffer: Vec::new(),
            locked: false,
            last_sequence: None,
            clock,
            stats: DecoderStats::default(),
        }
    }

    fn sync_at(&self, position: usize) -> bool {
        self.buffer[position..].starts_with(&self.layout.sync)
    }

    /// Offsets of complete frames, and how much of the buffer is consumed.
    /// A frame is trusted once the next one's sync confirms the length, or
    /// straight away if it follows a confirmed frame directly.
    fn extract(&mut self) -> (Vec<usize>, usize) {
        let size = self.layout.size;
        if size == 0 {
            return (Vec::new(), self.buffer.len());
        }
        if self.layout.sync.is_empty() {
            let count = self.buffer.len() / size;
            let frames = (0..count).map(|i| i * size).collect();
            return (frames, count * size);
        }
        let sync_length = self.layout.sync.len();
        let origin = self.layout.sync_offset;
        let mut frames = Vec::new();
        let mut position = 0;
        loop {
            if position + origin + sync_length > self.buffer.len() {
                return (frames, position);
            }
            if !self.sync_at(position + origin) {
                if self.locked {
                    self.locked = false;
                    self.stats.resyncs += 1;
                }
                position += 1;
                continue;
            }
            if position + size > self.buffer.len() {
                return (frames, position);
            }
            if !self.locked {
                let next = position + size + origin;
                if next + sync_length > self.buffer.len() {
                    return (frames, position);
                }
                if !self.sync_at(next) {
                    position += 1;
                    continue;
                }
                self.locked = true;
            }
            frames.push(position);
            position += size;
        }
    }

    fn note_sequence(&mut self, start: usize) {
        let Some((offset, scalar)) = self.layout.sequence else {
            return;
        };
        let value = scalar.decode(&self.buffer[start + offset..]) as u64;
        if let Some(last) = self.last_sequence {
            let mask = if scalar.size >= 8 {
                u64::MAX
            } else {
                (1u64 << (scalar.size * 8)) - 1
            };
            let delta = value.wrapping_sub(last) & mask;
            self.stats.lost += delta.saturating_sub(1);
        }
        self.last_sequence = Some(value);
    }
}

impl Decoder for StructDecoder {
    fn feed(&mut self, data: &[u8], t: i64, sink: &mut dyn Sink) {
        if data.is_empty() {
            if let Some(clock) = &mut self.clock {
                clock.note_empty_read();
            }
            return;
        }
        self.buffer.extend_from_slice(data);
        let (frames, consumed) = self.extract();
        if !frames.is_empty() {
            let times = match (&mut self.clock, &self.layout.timestamp) {
                (Some(clock), Some((offset, scalar, _))) => {
                    let raw: Vec<u32> = frames
                        .iter()
                        .map(|start| scalar.decode(&self.buffer[start + offset..]) as u32)
                        .collect();
                    clock.convert(&raw, t)
                }
                _ => vec![t; frames.len()],
            };
            for (start, time) in frames.iter().zip(times) {
                self.note_sequence(*start);
                self.stats.frames += 1;
                for field in &self.layout.fields {
                    let value = field.scalar.decode(&self.buffer[start + field.offset..]);
                    sink.sample(&field.name, field.group.as_deref(), field.plot, time, value);
                }
            }
        }
        self.buffer.drain(..consumed);
    }

    fn stats(&self) -> DecoderStats {
        self.stats
    }
}

/// Reads as zero: resolving a frame's fields never needs target memory.
struct NoMemory;

impl MemoryReader for NoMemory {
    fn read(&mut self, _addr: u32, buf: &mut [u8]) -> Result<(), String> {
        buf.fill(0);
        Ok(())
    }
}

/// Work out a frame's layout from its type in the ELF.
pub fn layout_from_elf(
    db: &SymbolDb,
    channel: &str,
    config: &StructConfig,
) -> Result<FrameLayout, String> {
    build_layout_from_elf(
        db,
        channel,
        &config.type_name,
        Some(config.sync),
        config.timestamp.as_ref(),
        config.timeline.as_deref(),
    )
}

fn build_layout_from_elf(
    db: &SymbolDb,
    channel: &str,
    type_name: &str,
    sync_word: Option<u32>,
    timestamp_cfg: Option<&Timestamp>,
    timeline_cfg: Option<&[TimelineEntry]>,
) -> Result<FrameLayout, String> {
    let ty = db
        .type_by_name(type_name)
        .ok_or_else(|| format!("no type named {type_name} in the ELF"))?;
    let size = db.size_of(ty, 0).map_err(|e| e.to_string())? as usize;
    let rules = Visualizers::with_builtin().ok();
    let leaves = leaves_of_place(
        db,
        rules.as_ref(),
        channel,
        Place::new(0, ty),
        &mut NoMemory,
    );
    let prefix = format!("{channel}.");
    let short = |name: &str| name.strip_prefix(&prefix).unwrap_or(name).to_owned();
    let find = |wanted: &str| {
        leaves
            .iter()
            .find(|leaf| short(&leaf.name) == wanted)
            .map(|leaf| (leaf.place.addr as usize, leaf.scalar))
    };

    let sync = match sync_word {
        Some(word) => {
            let sync_bytes = if word > 0xFFFF { 4 } else { 2 };
            word.to_le_bytes()[..sync_bytes].to_vec()
        }
        None => Vec::new(),
    };
    let sync_offset = if sync.is_empty() {
        0
    } else {
        find("sync").map_or(0, |(offset, _)| offset)
    };
    let sequence = find("seq");
    let timestamp = match timestamp_cfg {
        Some(stamp) => {
            let (offset, scalar) = find(&stamp.field)
                .ok_or_else(|| format!("{type_name} has no field {}", stamp.field))?;
            Some((offset, scalar, 1e9 / stamp.clock_hz))
        }
        None => None,
    };

    let selected: Option<Vec<(Vec<String>, Option<String>)>> =
        timeline_cfg.map(|entries| entries.iter().map(|entry| entry.expand()).collect());
    let mut fields = Vec::with_capacity(leaves.len());
    for leaf in &leaves {
        let name = short(&leaf.name);
        if name == "sync" || name == "seq" {
            continue;
        }
        let choice = selected
            .as_ref()
            .map(|list| list.iter().find(|(names, _)| names.contains(&name)));
        let (plot, group) = match choice {
            None => (true, None),
            Some(None) => (false, None),
            Some(Some((_, group))) => (true, group.clone()),
        };
        fields.push(FrameField {
            name: leaf.name.clone(),
            offset: leaf.place.addr as usize,
            scalar: leaf.scalar,
            plot,
            group,
        });
    }
    Ok(FrameLayout {
        size,
        sync,
        sync_offset,
        sequence,
        timestamp,
        fields,
    })
}

fn is_cpp_type_ident(name: &str) -> bool {
    !name.is_empty()
        && name.split("::").all(|seg| {
            let mut chars = seg.chars();
            let Some(first) = chars.next() else {
                return false;
            };
            (first.is_ascii_alphabetic() || first == '_')
                && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
        })
}

/// Parse an unconfigured RTT channel name as `Type`, `struct:Type`, or `Type@0xA55A`.
pub fn parse_struct_channel(name: &str) -> Option<(&str, Option<u32>)> {
    let body = name.strip_prefix("struct:").unwrap_or(name);
    let (ty, sync) = match body.split_once('@') {
        Some((ty, raw_sync)) => {
            let hex = raw_sync
                .strip_prefix("0x")
                .or_else(|| raw_sync.strip_prefix("0X"))?;
            if hex.is_empty() || hex.len() > 8 {
                return None;
            }
            let sync = u32::from_str_radix(hex, 16).ok()?;
            (ty, Some(sync))
        }
        None => (body, None),
    };
    is_cpp_type_ident(ty).then_some((ty, sync))
}

fn struct_definition(db: &SymbolDb, type_name: &str) -> Option<TypeRef> {
    let ty = db.type_by_name(type_name)?;
    let (resolved, info) = db.strip(ty).ok()?;
    match info.kind {
        TypeKind::Struct {
            declaration: false, ..
        } => Some(resolved),
        _ => None,
    }
}

/// Pick an automatic decoder for an RTT up-channel omitted from `.haltd/rtt.yaml`.
pub fn auto_decoder(
    db: &SymbolDb,
    channel_name: &str,
    index: u32,
) -> (Box<dyn Decoder>, &'static str, Option<String>) {
    if let Some((type_name, sync)) = parse_struct_channel(channel_name) {
        if let Some(ty) = struct_definition(db, type_name) {
            if sync.is_none() && db.find_member(ty, "sync").ok().flatten().is_some() {
                return (
                    Box::new(TextDecoder),
                    "text",
                    Some(format!(
                        "channel {index}: {type_name} has a sync field; name the channel {type_name}@0x<value>"
                    )),
                );
            }
            match build_layout_from_elf(db, channel_name, type_name, sync, None, None) {
                Ok(layout) if !layout.fields.is_empty() && layout.size > 0 => {
                    let problem = sync.is_none().then(|| {
                        format!(
                            "channel {index}: {type_name} framed by size only; use SEGGER_RTT_MODE_NO_BLOCK_SKIP"
                        )
                    });
                    return (Box::new(StructDecoder::new(layout)), "auto-struct", problem);
                }
                Ok(_) => {
                    return (
                        Box::new(TextDecoder),
                        "text",
                        Some(format!(
                            "channel {index}: {type_name} has no numeric fields to decode"
                        )),
                    );
                }
                Err(error) => {
                    return (
                        Box::new(TextDecoder),
                        "text",
                        Some(format!("channel {index}: {error}")),
                    );
                }
            }
        }
    }
    (
        Box::new(KeyValueDecoder::new(channel_name)),
        "auto-text",
        None,
    )
}

/// A user script: `decode(bytes, state) -> { {name, t, value}, ... }`. `t` is
/// optional, in seconds relative to the moment the bytes were received.
pub struct LuaDecoder {
    lua: Lua,
    calls: u64,
}

impl LuaDecoder {
    pub fn new(path: &Path) -> Result<Self, String> {
        let source = std::fs::read_to_string(path)
            .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
        let lua = Lua::new();
        lua.load(source)
            .set_name(path.display().to_string())
            .exec()
            .map_err(|error| error.to_string())?;
        let state = lua.create_table().map_err(|error| error.to_string())?;
        lua.globals()
            .set("__state", state)
            .map_err(|error| error.to_string())?;
        lua.globals()
            .get::<Function>("decode")
            .map_err(|_| "the script does not define decode(bytes, state)".to_owned())?;
        Ok(Self { lua, calls: 0 })
    }

    fn run(&self, data: &[u8], t: i64, sink: &mut dyn Sink) -> mlua::Result<()> {
        let decode: Function = self.lua.globals().get("decode")?;
        let state: Table = self.lua.globals().get("__state")?;
        let bytes = self.lua.create_string(data)?;
        let result: Value = decode.call((bytes, state))?;
        let Value::Table(entries) = result else {
            return Ok(()); // Nothing complete yet.
        };
        for entry in entries.sequence_values::<Table>() {
            let entry = entry?;
            let name: String = entry.get(1)?;
            let offset: Option<f64> = entry.get(2)?;
            let value: f64 = entry.get(3)?;
            let time = t + offset.map_or(0, |seconds| (seconds * 1e9) as i64);
            sink.sample(&name, None, true, time, value);
        }
        Ok(())
    }
}

impl Decoder for LuaDecoder {
    fn feed(&mut self, data: &[u8], t: i64, sink: &mut dyn Sink) {
        if data.is_empty() {
            return;
        }
        self.calls += 1;
        if let Err(error) = self.run(data, t, sink) {
            tracing::warn!("RTT Lua decoder failed: {error}");
        }
    }

    fn stats(&self) -> DecoderStats {
        DecoderStats {
            frames: self.calls,
            ..DecoderStats::default()
        }
    }
}

/// Saves the raw stream for SystemView to open.
pub struct SystemViewDecoder {
    out: BufWriter<File>,
    written: u64,
}

impl SystemViewDecoder {
    pub fn new(path: &Path) -> Result<Self, String> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
        }
        let file = File::create(path)
            .map_err(|error| format!("cannot create {}: {error}", path.display()))?;
        Ok(Self {
            out: BufWriter::new(file),
            written: 0,
        })
    }
}

impl Decoder for SystemViewDecoder {
    fn feed(&mut self, data: &[u8], _t: i64, _sink: &mut dyn Sink) {
        if data.is_empty() {
            let _ = self.out.flush();
            return;
        }
        if self.out.write_all(data).is_ok() {
            self.written += data.len() as u64;
        }
    }

    fn stats(&self) -> DecoderStats {
        DecoderStats {
            frames: self.written,
            ..DecoderStats::default()
        }
    }
}

/// Build the decoder a channel's configuration asks for.
pub fn build(
    db: &SymbolDb,
    workspace: Option<&Path>,
    channel: &str,
    decode: &Decode,
) -> Result<Box<dyn Decoder>, String> {
    let resolve = |file: &str| match workspace {
        Some(base) => base.join(file),
        None => file.into(),
    };
    Ok(match decode {
        Decode::Text { .. } => Box::new(TextDecoder),
        Decode::TextRegex { pattern, group } => {
            Box::new(RegexDecoder::new(channel, pattern, group.clone())?)
        }
        Decode::Struct(config) => {
            Box::new(StructDecoder::new(layout_from_elf(db, channel, config)?))
        }
        Decode::Lua { script } => Box::new(LuaDecoder::new(&resolve(script))?),
        Decode::Systemview { file } => Box::new(SystemViewDecoder::new(&resolve(file))?),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use sampler::codec::ScalarKind;

    #[derive(Default)]
    struct Collect {
        samples: Vec<(String, i64, f64, bool)>,
        text: Vec<u8>,
    }

    impl Sink for Collect {
        fn sample(&mut self, name: &str, _group: Option<&str>, plot: bool, t: i64, value: f64) {
            self.samples.push((name.to_owned(), t, value, plot));
        }
        fn text(&mut self, data: &[u8]) {
            self.text.extend_from_slice(data);
        }
    }

    fn unsigned(size: u32) -> Scalar {
        Scalar {
            kind: ScalarKind::Unsigned,
            size,
            bits: None,
        }
    }

    /// sync:u16, seq:u16, value:u32
    fn layout() -> FrameLayout {
        FrameLayout {
            size: 8,
            sync: vec![0x5A, 0xA5],
            sync_offset: 0,
            sequence: Some((2, unsigned(2))),
            timestamp: None,
            fields: vec![FrameField {
                name: "v".to_owned(),
                offset: 4,
                scalar: unsigned(4),
                plot: true,
                group: None,
            }],
        }
    }

    fn frame(sequence: u16, value: u32) -> Vec<u8> {
        let mut out = vec![0x5A, 0xA5];
        out.extend_from_slice(&sequence.to_le_bytes());
        out.extend_from_slice(&value.to_le_bytes());
        out
    }

    fn values(sink: &Collect) -> Vec<f64> {
        sink.samples.iter().map(|sample| sample.2).collect()
    }

    fn stream(frames: &[(u16, u32)]) -> Vec<u8> {
        frames.iter().flat_map(|(s, v)| frame(*s, *v)).collect()
    }

    #[test]
    fn frames_split_across_reads_are_reassembled() {
        let mut decoder = StructDecoder::new(layout());
        let mut sink = Collect::default();
        let data = stream(&[(1, 10), (2, 20), (3, 30)]);
        for piece in data.chunks(3) {
            decoder.feed(piece, 0, &mut sink);
        }
        assert_eq!(values(&sink), [10.0, 20.0, 30.0]);
        assert_eq!(decoder.stats().lost, 0);
    }

    #[test]
    fn the_first_frame_waits_for_confirmation() {
        let mut decoder = StructDecoder::new(layout());
        let mut sink = Collect::default();
        decoder.feed(&frame(1, 10), 0, &mut sink);
        assert!(sink.samples.is_empty());
        decoder.feed(&frame(2, 20), 0, &mut sink);
        assert_eq!(values(&sink), [10.0, 20.0]);
    }

    #[test]
    fn garbage_before_a_frame_is_skipped() {
        let mut decoder = StructDecoder::new(layout());
        let mut sink = Collect::default();
        // Contains a false sync whose "next frame" does not line up.
        let mut data = vec![1, 2, 0x5A, 0xA5, 3, 4, 5];
        data.extend(stream(&[(7, 70), (8, 80)]));
        decoder.feed(&data, 0, &mut sink);
        assert_eq!(values(&sink), [70.0, 80.0]);
    }

    #[test]
    fn a_broken_stream_resynchronizes() {
        let mut decoder = StructDecoder::new(layout());
        let mut sink = Collect::default();
        let mut data = stream(&[(1, 10), (2, 20)]);
        data.extend_from_slice(&[9, 9, 9]); // Part of an overwritten frame.
        data.extend(stream(&[(5, 50), (6, 60), (7, 70)]));
        decoder.feed(&data, 0, &mut sink);
        assert_eq!(values(&sink), [10.0, 20.0, 50.0, 60.0, 70.0]);
        assert_eq!(decoder.stats().resyncs, 1);
        assert_eq!(decoder.stats().lost, 2);
    }

    #[test]
    fn sequence_numbers_may_wrap() {
        let mut decoder = StructDecoder::new(layout());
        let mut sink = Collect::default();
        decoder.feed(&stream(&[(65535, 1), (0, 2), (1, 3)]), 0, &mut sink);
        assert_eq!(decoder.stats().lost, 0);
    }

    #[test]
    fn regex_lines_become_signals_and_still_reach_the_terminal() {
        let mut decoder =
            RegexDecoder::new("ch", r"pos=(?P<pos>-?\d+) vel=(?P<vel>[\d.]+)", None).unwrap();
        let mut sink = Collect::default();
        decoder.feed(b"pos=-3 ve", 5, &mut sink);
        assert!(sink.samples.is_empty());
        decoder.feed(b"l=1.5\r\nother\n", 6, &mut sink);
        assert_eq!(sink.samples.len(), 2);
        assert_eq!(sink.samples[0], ("ch.pos".to_owned(), 6, -3.0, true));
        assert_eq!(sink.samples[1].2, 1.5);
        assert!(sink.text.starts_with(b"pos=-3 vel"));
    }

    #[test]
    fn a_pattern_without_names_is_rejected() {
        assert!(RegexDecoder::new("c", r"\d+", None).is_err());
        assert!(RegexDecoder::new("c", r"(?P<a>", None).is_err());
    }

    fn script(name: &str, text: &str) -> std::path::PathBuf {
        let path =
            std::env::temp_dir().join(format!("haltd-rtt-{}-{name}.lua", std::process::id()));
        std::fs::write(&path, text).unwrap();
        path
    }

    #[test]
    fn lua_scripts_keep_state_between_reads() {
        // Two-byte records, which may arrive split.
        let path = script(
            "state",
            r#"
            function decode(bytes, state)
              local data = (state.rest or "") .. bytes
              local out = {}
              while #data >= 2 do
                out[#out + 1] = { "m.value", nil, data:byte(1) + data:byte(2) * 256 }
                data = data:sub(3)
              end
              state.rest = data
              return out
            end
            "#,
        );
        let mut decoder = LuaDecoder::new(&path).unwrap();
        let mut sink = Collect::default();
        decoder.feed(&[1, 1, 2], 100, &mut sink);
        decoder.feed(&[1], 200, &mut sink);
        assert_eq!(
            sink.samples,
            [
                ("m.value".to_owned(), 100, 257.0, true),
                ("m.value".to_owned(), 200, 258.0, true),
            ]
        );
    }

    #[test]
    fn a_lua_error_does_not_stop_the_stream() {
        let path = script("bad", r#"function decode() error("boom") end"#);
        let mut decoder = LuaDecoder::new(&path).unwrap();
        let mut sink = Collect::default();
        decoder.feed(&[1], 0, &mut sink);
        decoder.feed(&[2], 0, &mut sink);
        assert_eq!(decoder.stats().frames, 2);
    }

    #[test]
    fn a_script_without_decode_is_rejected() {
        let path = script("none", "x = 1");
        assert!(LuaDecoder::new(&path).is_err());
    }

    #[test]
    fn systemview_data_is_written_verbatim() {
        let path = std::env::temp_dir().join(format!("haltd-rtt-{}.SVDat", std::process::id()));
        {
            let mut decoder = SystemViewDecoder::new(&path).unwrap();
            let mut sink = Collect::default();
            decoder.feed(&[1, 2, 3], 0, &mut sink);
            decoder.feed(&[4], 0, &mut sink);
            decoder.feed(&[], 0, &mut sink);
        }
        assert_eq!(std::fs::read(&path).unwrap(), [1, 2, 3, 4]);
    }

    #[test]
    fn struct_channel_names_parse_sync_and_prefix() {
        assert_eq!(
            parse_struct_channel("TelemetryFrame"),
            Some(("TelemetryFrame", None))
        );
        assert_eq!(
            parse_struct_channel("struct:fw::Telemetry"),
            Some(("fw::Telemetry", None))
        );
        assert_eq!(
            parse_struct_channel("TelemetryFrame@0xA55A"),
            Some(("TelemetryFrame", Some(0xA55A)))
        );
        assert_eq!(
            parse_struct_channel("struct:fw::Frame@0X1234ABCD"),
            Some(("fw::Frame", Some(0x1234_ABCD)))
        );
        assert_eq!(parse_struct_channel("Channel 0"), None);
        assert_eq!(parse_struct_channel("Telemetry@bad"), None);
        assert_eq!(parse_struct_channel("Telemetry@0x"), None);
        assert_eq!(parse_struct_channel("@0x1234"), None);
        assert_eq!(parse_struct_channel("Telemetry@1234"), None);
    }

    #[test]
    fn sync_less_struct_decoder_splits_consecutive_frames() {
        let layout = FrameLayout {
            size: 4,
            sync: Vec::new(),
            sync_offset: 0,
            sequence: None,
            timestamp: None,
            fields: vec![FrameField {
                name: "Frame.v".to_owned(),
                offset: 0,
                scalar: unsigned(4),
                plot: true,
                group: None,
            }],
        };
        let mut decoder = StructDecoder::new(layout);
        let mut sink = Collect::default();
        let mut stream = Vec::new();
        stream.extend_from_slice(&10u32.to_le_bytes());
        stream.extend_from_slice(&20u32.to_le_bytes());
        stream.extend_from_slice(&30u32.to_le_bytes());
        // Feed misaligned chunks (5 bytes, then 3 bytes, then 4 bytes).
        decoder.feed(&stream[..5], 100, &mut sink);
        assert_eq!(values(&sink), [10.0]);
        decoder.feed(&stream[5..8], 200, &mut sink);
        assert_eq!(values(&sink), [10.0, 20.0]);
        decoder.feed(&stream[8..], 300, &mut sink);
        assert_eq!(values(&sink), [10.0, 20.0, 30.0]);
        assert_eq!(decoder.stats().frames, 3);
    }

    #[test]
    fn kv_decoder_extracts_numbers_forwards_text_and_caps_at_32() {
        let mut decoder = KeyValueDecoder::new("log");
        let mut sink = Collect::default();
        decoder.feed(b"\x1b[32mspeed=-12.5\x1b[0m, err: 1.5e-2", 10, &mut sink);
        assert!(sink.samples.is_empty());
        decoder.feed(b" addr=0x2000 ver=1.2.3\r\n", 20, &mut sink);
        assert_eq!(
            sink.samples,
            [
                ("log.speed".to_owned(), 20, -12.5, true),
                ("log.err".to_owned(), 20, 0.015, true),
            ]
        );
        assert!(sink
            .text
            .starts_with(b"\x1b[32mspeed=-12.5\x1b[0m, err: 1.5e-2"));

        let mut capped = KeyValueDecoder::new("cap");
        let mut cap_sink = Collect::default();
        let mut line = String::new();
        for i in 0..40 {
            line.push_str(&format!("k{i}={i} "));
        }
        line.push('\n');
        capped.feed(line.as_bytes(), 50, &mut cap_sink);
        assert_eq!(cap_sink.samples.len(), MAX_KV_SIGNALS);
        assert_eq!(cap_sink.samples[0].0, "cap.k0");
        assert_eq!(cap_sink.samples[31].0, "cap.k31");
        // Previously seen names still update after reaching the cap.
        capped.feed(b"k0=99 k39=1\n", 60, &mut cap_sink);
        assert_eq!(cap_sink.samples.len(), MAX_KV_SIGNALS + 1);
        assert_eq!(cap_sink.samples.last().unwrap().2, 99.0);
    }
}
