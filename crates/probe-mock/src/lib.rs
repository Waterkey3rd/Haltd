//! Simulated probe for CI: a memory image with scripted run control, plus
//! recording and replay of real probe sessions (DESIGN §14).

mod record;

use std::collections::{BTreeMap, VecDeque};
use std::path::Path;
use std::time::Duration;

use probe_api::{
    AccessWidth, BpHandle, BpKind, ConnectCfg, DownloadStats, HssApi, HssBlock, HssCaps, Probe,
    ProbeError, ProbeInfo, Progress, Reg, ResetKind, Result, RttApi, RttChannel, RttDirection,
    RttStatus, SwoApi, TargetInfo, WatchpointSpec, WpHandle, HSS_FLAG_TIMESTAMP_US,
    HSS_RECORD_HEADER_BYTES,
};

pub use record::{Op, RecordingProbe, ReplayProbe};

const HW_BREAKPOINTS: usize = 8;
const HW_WATCHPOINTS: usize = 4;

struct Region {
    base: u32,
    data: Vec<u8>,
}

struct HssRun {
    blocks: Vec<HssBlock>,
    period_us: u32,
    timestamps: bool,
    sequence: u32,
}

/// Makes every `hss_read` deliver records while a counter in memory advances,
/// standing in for a free-running DWT_CYCCNT.
#[derive(Clone, Copy)]
struct HssAuto {
    records_per_read: u32,
    counter_addr: u32,
    step: u32,
}

/// In-memory target. Everything is deterministic and driven by the test.
#[derive(Default)]
pub struct MockProbe {
    regions: Vec<Region>,
    regs: BTreeMap<Reg, u32>,
    halted: bool,
    /// Each `go` pops one entry: `is_halted` polls until the target stops.
    halt_script: VecDeque<u32>,
    polls_until_halt: Option<u32>,
    breakpoints: BTreeMap<u32, (u32, BpKind)>,
    watchpoints: BTreeMap<u32, WatchpointSpec>,
    next_handle: u32,
    hss: Option<HssRun>,
    pending_hss_records: u32,
    hss_auto: Option<HssAuto>,
    rtt_running: bool,
    rtt_up: BTreeMap<u32, VecDeque<u8>>,
    rtt_down: BTreeMap<u32, Vec<u8>>,
    connected: bool,
}

impl MockProbe {
    pub fn new() -> Self {
        Self {
            next_handle: 1,
            ..Self::default()
        }
    }

    pub fn add_region(&mut self, base: u32, data: Vec<u8>) {
        self.regions.push(Region { base, data });
    }

    pub fn set_reg(&mut self, reg: Reg, value: u32) {
        self.regs.insert(reg, value);
    }

    /// After the next `go`, report halted on the n-th `is_halted` poll.
    pub fn push_halt_after_polls(&mut self, polls: u32) {
        self.halt_script.push_back(polls);
    }

    /// Make the next `hss_read` deliver this many records.
    pub fn queue_hss_records(&mut self, records: u32) {
        self.pending_hss_records += records;
    }

    /// See [`HssAuto`]: each record sees `counter_addr` advanced by `step`.
    pub fn set_hss_auto(&mut self, records_per_read: u32, counter_addr: u32, step: u32) {
        self.hss_auto = Some(HssAuto {
            records_per_read,
            counter_addr,
            step,
        });
    }

    pub fn push_rtt_up(&mut self, channel: u32, data: &[u8]) {
        self.rtt_up.entry(channel).or_default().extend(data);
    }

    pub fn rtt_down(&self, channel: u32) -> &[u8] {
        self.rtt_down.get(&channel).map_or(&[], Vec::as_slice)
    }

    pub fn breakpoints(&self) -> Vec<(u32, BpKind)> {
        self.breakpoints.values().copied().collect()
    }

    fn range(&self, addr: u32, len: usize) -> Result<(usize, usize)> {
        for (index, region) in self.regions.iter().enumerate() {
            let Some(offset) = addr.checked_sub(region.base) else {
                continue;
            };
            let offset = offset as usize;
            if offset + len <= region.data.len() {
                return Ok((index, offset));
            }
        }
        Err(ProbeError::Memory {
            addr,
            len,
            detail: " (unmapped in mock)".to_owned(),
        })
    }

    fn take_handle(&mut self) -> u32 {
        let handle = self.next_handle;
        self.next_handle += 1;
        handle
    }
}

impl Probe for MockProbe {
    fn info(&self) -> ProbeInfo {
        ProbeInfo {
            product: "Mock Probe".to_owned(),
            serial: 1,
            firmware: "mock".to_owned(),
            ..ProbeInfo::default()
        }
    }

    fn connect(&mut self, cfg: &ConnectCfg) -> Result<TargetInfo> {
        self.connected = true;
        Ok(TargetInfo {
            device: cfg.device.clone(),
            speed_khz: cfg.speed_khz,
            cpuid: 0x411F_C271,
            hw_breakpoints: HW_BREAKPOINTS as u32,
            halted: self.halted,
        })
    }

    fn disconnect(&mut self) -> Result<()> {
        self.connected = false;
        Ok(())
    }

    fn halt(&mut self) -> Result<()> {
        self.halted = true;
        self.polls_until_halt = None;
        Ok(())
    }

    fn go(&mut self) -> Result<()> {
        self.halted = false;
        self.polls_until_halt = self.halt_script.pop_front();
        Ok(())
    }

    fn step_instr(&mut self) -> Result<()> {
        if !self.halted {
            return Err(ProbeError::Other("step while running".to_owned()));
        }
        *self.regs.entry(Reg::PC).or_insert(0) += 2;
        Ok(())
    }

    fn reset(&mut self, _kind: ResetKind, halt_after: bool) -> Result<()> {
        self.halted = halt_after;
        Ok(())
    }

    fn is_halted(&mut self) -> Result<bool> {
        if let Some(polls) = self.polls_until_halt.as_mut() {
            *polls = polls.saturating_sub(1);
            if *polls == 0 {
                self.polls_until_halt = None;
                self.halted = true;
            }
        }
        Ok(self.halted)
    }

    fn read_regs(&mut self, ids: &[Reg], out: &mut [u32]) -> Result<()> {
        for (id, slot) in ids.iter().zip(out.iter_mut()) {
            *slot = self.regs.get(id).copied().unwrap_or(0);
        }
        Ok(())
    }

    fn write_reg(&mut self, id: Reg, v: u32) -> Result<()> {
        self.regs.insert(id, v);
        Ok(())
    }

    fn read_mem(&mut self, addr: u32, buf: &mut [u8], _width: AccessWidth) -> Result<()> {
        let (index, offset) = self.range(addr, buf.len())?;
        buf.copy_from_slice(&self.regions[index].data[offset..offset + buf.len()]);
        Ok(())
    }

    fn write_mem(&mut self, addr: u32, data: &[u8], _width: AccessWidth) -> Result<()> {
        let (index, offset) = self.range(addr, data.len())?;
        self.regions[index].data[offset..offset + data.len()].copy_from_slice(data);
        Ok(())
    }

    fn set_bp(&mut self, addr: u32, kind: BpKind) -> Result<BpHandle> {
        let used = self
            .breakpoints
            .values()
            .filter(|(_, k)| *k == BpKind::Hw)
            .count();
        let actual = match kind {
            BpKind::Flash => BpKind::Flash,
            BpKind::Hw | BpKind::Any if used < HW_BREAKPOINTS => BpKind::Hw,
            BpKind::Any => BpKind::Flash,
            BpKind::Hw => {
                return Err(ProbeError::Other(
                    "all hardware breakpoints in use".to_owned(),
                ))
            }
        };
        let handle = self.take_handle();
        self.breakpoints.insert(handle, (addr, actual));
        Ok(BpHandle(handle))
    }

    fn clear_bp(&mut self, h: BpHandle) -> Result<()> {
        self.breakpoints
            .remove(&h.0)
            .map(|_| ())
            .ok_or_else(|| ProbeError::Other(format!("unknown breakpoint {}", h.0)))
    }

    fn set_wp(&mut self, w: &WatchpointSpec) -> Result<WpHandle> {
        if self.watchpoints.len() >= HW_WATCHPOINTS {
            return Err(ProbeError::Other("all watchpoints in use".to_owned()));
        }
        let handle = self.take_handle();
        self.watchpoints.insert(handle, w.clone());
        Ok(WpHandle(handle))
    }

    fn clear_wp(&mut self, h: WpHandle) -> Result<()> {
        self.watchpoints
            .remove(&h.0)
            .map(|_| ())
            .ok_or_else(|| ProbeError::Other(format!("unknown watchpoint {}", h.0)))
    }

    fn download(
        &mut self,
        elf_or_bin: &Path,
        progress: &dyn Fn(Progress),
    ) -> Result<DownloadStats> {
        progress(Progress {
            action: "Program".to_owned(),
            item: elf_or_bin.display().to_string(),
            percent: 100,
        });
        Ok(DownloadStats {
            elapsed: Duration::ZERO,
        })
    }

    fn hss(&mut self) -> Option<&mut dyn HssApi> {
        Some(self)
    }

    fn rtt(&mut self) -> Option<&mut dyn RttApi> {
        Some(self)
    }

    fn swo(&mut self) -> Option<&mut dyn SwoApi> {
        None
    }

    fn set_speed(&mut self, khz: u32) -> Result<u32> {
        Ok(khz)
    }
}

impl HssApi for MockProbe {
    fn hss_caps(&mut self) -> Result<HssCaps> {
        Ok(HssCaps {
            max_blocks: 8,
            max_freq_hz: 1_000_000,
            caps: 0,
            raw: [0; 8],
        })
    }

    fn hss_start(&mut self, blocks: &[HssBlock], period_us: u32, flags: u32) -> Result<()> {
        self.hss = Some(HssRun {
            blocks: blocks.to_vec(),
            period_us,
            timestamps: flags & HSS_FLAG_TIMESTAMP_US != 0,
            sequence: 0,
        });
        Ok(())
    }

    fn hss_read(&mut self, buf: &mut [u8]) -> Result<usize> {
        let run = self
            .hss
            .as_ref()
            .ok_or_else(|| ProbeError::Other("HSS has not been started".to_owned()))?;
        let payload: usize = run.blocks.iter().map(|b| b.num_bytes as usize).sum();
        let record_size = payload + HSS_RECORD_HEADER_BYTES;
        let capacity = u32::try_from(buf.len() / record_size).unwrap_or(u32::MAX);
        let auto = self.hss_auto;
        if let Some(auto) = auto {
            self.pending_hss_records += auto.records_per_read;
        }
        let records = self.pending_hss_records.min(capacity);
        self.pending_hss_records -= records;

        let blocks = run.blocks.clone();
        let (period_us, timestamps) = (run.period_us, run.timestamps);
        let mut sequence = run.sequence;
        let mut written = 0;
        for _ in 0..records {
            if let Some(auto) = auto {
                let (region, offset) = self.range(auto.counter_addr, 4)?;
                let bytes = &mut self.regions[region].data[offset..offset + 4];
                let value = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
                bytes.copy_from_slice(&value.wrapping_add(auto.step).to_le_bytes());
            }
            let header = if timestamps {
                sequence.wrapping_mul(period_us)
            } else {
                sequence
            };
            buf[written..written + 4].copy_from_slice(&header.to_le_bytes());
            written += HSS_RECORD_HEADER_BYTES;
            for block in &blocks {
                let (region, offset) = self.range(block.addr, block.num_bytes as usize)?;
                let end = offset + block.num_bytes as usize;
                buf[written..written + block.num_bytes as usize]
                    .copy_from_slice(&self.regions[region].data[offset..end]);
                written += block.num_bytes as usize;
            }
            sequence = sequence.wrapping_add(1);
        }
        if let Some(run) = self.hss.as_mut() {
            run.sequence = sequence;
        }
        Ok(written)
    }

    fn hss_stop(&mut self) -> Result<()> {
        self.hss = None;
        Ok(())
    }
}

impl RttApi for MockProbe {
    fn rtt_start(&mut self, _control_block: Option<u32>) -> Result<()> {
        self.rtt_running = true;
        Ok(())
    }

    fn rtt_stop(&mut self) -> Result<()> {
        self.rtt_running = false;
        Ok(())
    }

    fn rtt_num_buffers(&mut self, _dir: RttDirection) -> Result<Option<u32>> {
        Ok(self.rtt_running.then_some(3))
    }

    fn rtt_channel(&mut self, dir: RttDirection, index: u32) -> Result<RttChannel> {
        Ok(RttChannel {
            index,
            direction: dir,
            name: format!("mock{index}"),
            size: 1024,
            flags: 0,
        })
    }

    fn rtt_status(&mut self) -> Result<RttStatus> {
        Ok(RttStatus {
            running: self.rtt_running,
            up_buffers: 3,
            down_buffers: 3,
            ..RttStatus::default()
        })
    }

    fn rtt_read(&mut self, channel: u32, buf: &mut [u8]) -> Result<usize> {
        let queue = self.rtt_up.entry(channel).or_default();
        let count = buf.len().min(queue.len());
        for slot in &mut buf[..count] {
            *slot = queue.pop_front().unwrap_or_default();
        }
        Ok(count)
    }

    fn rtt_write(&mut self, channel: u32, data: &[u8]) -> Result<usize> {
        self.rtt_down.entry(channel).or_default().extend(data);
        Ok(data.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use probe_api::ProbeExt;

    #[test]
    fn memory_round_trips_and_faults_outside_regions() {
        let mut mock = MockProbe::new();
        mock.add_region(0x2000_0000, vec![0; 16]);
        mock.write_u32(0x2000_0004, 0xDEAD_BEEF).unwrap();
        assert_eq!(mock.read_u32(0x2000_0004).unwrap(), 0xDEAD_BEEF);
        assert!(mock.read_u32(0x2000_0010).is_err());
        assert!(mock.read_u32(0x1FFF_FFFC).is_err());
    }

    #[test]
    fn scripted_halt_fires_on_the_requested_poll() {
        let mut mock = MockProbe::new();
        mock.push_halt_after_polls(3);
        mock.go().unwrap();
        assert!(!mock.is_halted().unwrap());
        assert!(!mock.is_halted().unwrap());
        assert!(mock.is_halted().unwrap());
    }

    #[test]
    fn any_breakpoints_spill_to_flash_after_eight() {
        let mut mock = MockProbe::new();
        for i in 0..9 {
            mock.set_bp(0x0800_0000 + i * 4, BpKind::Any).unwrap();
        }
        let flash = mock
            .breakpoints()
            .iter()
            .filter(|(_, kind)| *kind == BpKind::Flash)
            .count();
        assert_eq!(flash, 1);
        assert!(mock.set_bp(0x0800_1000, BpKind::Hw).is_err());
    }

    #[test]
    fn hss_concatenates_blocks_with_optional_timestamp() {
        let mut mock = MockProbe::new();
        mock.add_region(0x2000_0000, (0u8..16).collect());
        let blocks = [
            HssBlock {
                addr: 0x2000_0000,
                num_bytes: 2,
            },
            HssBlock {
                addr: 0x2000_0008,
                num_bytes: 2,
            },
        ];
        mock.hss_start(&blocks, 1000, HSS_FLAG_TIMESTAMP_US)
            .unwrap();
        mock.queue_hss_records(2);
        let mut buf = [0u8; 64];
        assert_eq!(mock.hss_read(&mut buf).unwrap(), 16);
        assert_eq!(&buf[..8], &[0, 0, 0, 0, 0, 1, 8, 9]);
        assert_eq!(&buf[8..12], &1000u32.to_le_bytes());
    }

    #[test]
    fn hss_read_delivers_whole_records_only() {
        let mut mock = MockProbe::new();
        mock.add_region(0, vec![1; 16]);
        let block = HssBlock {
            addr: 0,
            num_bytes: 4,
        };
        mock.hss_start(&[block], 1000, 0).unwrap();
        mock.queue_hss_records(3);
        let mut small = [0u8; 17];
        assert_eq!(mock.hss_read(&mut small).unwrap(), 16);
        assert_eq!(mock.hss_read(&mut small).unwrap(), 8);
    }

    #[test]
    fn hss_header_is_a_sequence_number_without_the_timestamp_flag() {
        let mut mock = MockProbe::new();
        mock.add_region(0, vec![9; 4]);
        let block = HssBlock {
            addr: 0,
            num_bytes: 4,
        };
        mock.hss_start(&[block], 1000, 0).unwrap();
        mock.queue_hss_records(2);
        let mut buf = [0u8; 16];
        assert_eq!(mock.hss_read(&mut buf).unwrap(), 16);
        assert_eq!(&buf[..4], &0u32.to_le_bytes());
        assert_eq!(&buf[8..12], &1u32.to_le_bytes());
    }

    #[test]
    fn rtt_channels_queue_bytes_both_ways() {
        let mut mock = MockProbe::new();
        mock.push_rtt_up(0, b"hello");
        let mut buf = [0u8; 3];
        assert_eq!(mock.rtt_read(0, &mut buf).unwrap(), 3);
        assert_eq!(&buf, b"hel");
        mock.rtt_write(1, b"cmd").unwrap();
        assert_eq!(mock.rtt_down(1), b"cmd");
    }
}
