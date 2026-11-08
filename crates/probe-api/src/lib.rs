//! Probe abstraction shared by the J-Link backend, the mock probe and the
//! debugger core (DESIGN §5.1).

mod device;
mod stats;

use std::path::Path;
use std::time::Duration;

use serde::{Deserialize, Serialize};

pub use device::{
    apply_post_connect, enable_cycle_counter, DeviceProfile, MemoryRegion, PostConnectOptions,
    RegisterBit, RegisterWrite,
};
pub use stats::{CallStats, Histogram, HistogramSummary};

/// Default SWD clock. 4 MHz is the project's standard setting for the J-Link
/// V9; higher rates have to be proven stable by `jlbench` first.
pub const DEFAULT_SPEED_KHZ: u32 = 4000;

pub type Result<T, E = ProbeError> = std::result::Result<T, E>;

#[derive(Debug, thiserror::Error)]
pub enum ProbeError {
    #[error("not supported: {0}")]
    Unsupported(String),
    #[error("not connected to a target")]
    NotConnected,
    #[error("failed to load probe library: {0}")]
    Load(String),
    #[error("{func} failed (code {code}){detail}")]
    Call {
        func: &'static str,
        code: i32,
        /// Empty, or the probe library's last error prefixed with ": ".
        detail: String,
    },
    #[error("memory access of {len} bytes at {addr:#010x} failed{detail}")]
    Memory {
        addr: u32,
        len: usize,
        detail: String,
    },
    #[error("probe worker thread has stopped")]
    ActorStopped,
    #[error("{0}")]
    Other(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Interface {
    Jtag,
    Swd,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ConnectCfg {
    pub device: String,
    pub interface: Interface,
    pub speed_khz: u32,
}

impl Default for ConnectCfg {
    fn default() -> Self {
        Self {
            device: "STM32H723VG".to_owned(),
            interface: Interface::Swd,
            speed_khz: DEFAULT_SPEED_KHZ,
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ProbeInfo {
    pub product: String,
    pub serial: u32,
    pub firmware: String,
    pub hardware_version: String,
    pub dll_version: String,
    pub caps: u32,
    pub cap_names: Vec<String>,
    /// Extended capability bitmap, one byte per eight capabilities.
    pub caps_ex: Vec<u8>,
    /// Library symbols that were not found; the features using them degrade.
    pub missing_symbols: Vec<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct TargetInfo {
    pub device: String,
    pub speed_khz: u32,
    pub cpuid: u32,
    pub hw_breakpoints: u32,
    pub halted: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ResetKind {
    /// Default strategy of the probe (SYSRESETREQ on Cortex-M).
    Normal,
    /// Reset the core only, leaving peripherals untouched.
    Core,
    /// Pulse the nRESET pin.
    Pin,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Reg {
    /// R0..=R15.
    R(u8),
    Xpsr,
    Msp,
    Psp,
    Primask,
    Basepri,
    Faultmask,
    Control,
    Fpscr,
    /// S0..=S31.
    S(u8),
}

impl Reg {
    pub const SP: Reg = Reg::R(13);
    pub const LR: Reg = Reg::R(14);
    pub const PC: Reg = Reg::R(15);
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum AccessWidth {
    /// Let the probe pick the most efficient access size.
    #[default]
    Auto,
    U8,
    U16,
    U32,
}

impl AccessWidth {
    /// Width in bytes, or 0 for [`AccessWidth::Auto`].
    pub fn bytes(self) -> u32 {
        match self {
            AccessWidth::Auto => 0,
            AccessWidth::U8 => 1,
            AccessWidth::U16 => 2,
            AccessWidth::U32 => 4,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum BpKind {
    Hw,
    Flash,
    Any,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct BpHandle(pub u32);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct WpHandle(pub u32);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum WpAccess {
    Read,
    Write,
    ReadWrite,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WatchpointSpec {
    pub addr: u32,
    /// Bits set here are ignored when comparing addresses.
    pub addr_mask: u32,
    pub access: WpAccess,
}

#[derive(Clone, Debug, Default)]
pub struct Progress {
    pub action: String,
    pub item: String,
    pub percent: i32,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DownloadStats {
    pub elapsed: Duration,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HssBlock {
    pub addr: u32,
    pub num_bytes: u32,
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
pub struct HssCaps {
    pub max_blocks: u32,
    pub max_freq_hz: u32,
    pub caps: u32,
    /// All eight words the library returned, including undocumented ones.
    pub raw: [u32; 8],
}

/// Make each HSS record's header a microsecond timestamp instead of a
/// sequence number.
pub const HSS_FLAG_TIMESTAMP_US: u32 = 1;

/// Every HSS record is this header followed by the blocks in order. Measured
/// with jlbench on a J-Link V9 (firmware 2021-05-07, DLL V9.80).
pub const HSS_RECORD_HEADER_BYTES: usize = 4;

pub trait HssApi {
    fn hss_caps(&mut self) -> Result<HssCaps>;
    fn hss_start(&mut self, blocks: &[HssBlock], period_us: u32, flags: u32) -> Result<()>;
    /// Returns the number of bytes copied into `buf`.
    fn hss_read(&mut self, buf: &mut [u8]) -> Result<usize>;
    fn hss_stop(&mut self) -> Result<()>;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum RttDirection {
    Up,
    Down,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct RttStatus {
    pub bytes_transferred: u32,
    pub bytes_read: u32,
    pub host_overflows: i32,
    pub running: bool,
    pub up_buffers: i32,
    pub down_buffers: i32,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RttChannel {
    pub index: u32,
    pub direction: RttDirection,
    pub name: String,
    pub size: u32,
    pub flags: u32,
}

pub trait RttApi {
    /// Start the RTT engine, scanning for the control block if `addr` is None.
    fn rtt_start(&mut self, control_block: Option<u32>) -> Result<()>;
    fn rtt_stop(&mut self) -> Result<()>;
    /// Returns None while the control block has not been found yet.
    fn rtt_num_buffers(&mut self, dir: RttDirection) -> Result<Option<u32>>;
    fn rtt_channel(&mut self, dir: RttDirection, index: u32) -> Result<RttChannel>;
    fn rtt_status(&mut self) -> Result<RttStatus>;
    fn rtt_read(&mut self, channel: u32, buf: &mut [u8]) -> Result<usize>;
    fn rtt_write(&mut self, channel: u32, data: &[u8]) -> Result<usize>;
}

/// Reserved: the first target board has no usable SWO pin (DESIGN §2).
pub trait SwoApi {}

pub trait Probe: Send {
    fn info(&self) -> ProbeInfo;
    fn connect(&mut self, cfg: &ConnectCfg) -> Result<TargetInfo>;
    fn disconnect(&mut self) -> Result<()>;

    fn halt(&mut self) -> Result<()>;
    fn go(&mut self) -> Result<()>;
    fn step_instr(&mut self) -> Result<()>;
    fn reset(&mut self, kind: ResetKind, halt_after: bool) -> Result<()>;
    fn is_halted(&mut self) -> Result<bool>;

    fn read_regs(&mut self, ids: &[Reg], out: &mut [u32]) -> Result<()>;
    fn write_reg(&mut self, id: Reg, v: u32) -> Result<()>;
    fn read_mem(&mut self, addr: u32, buf: &mut [u8], width: AccessWidth) -> Result<()>;
    fn write_mem(&mut self, addr: u32, data: &[u8], width: AccessWidth) -> Result<()>;

    fn set_bp(&mut self, addr: u32, kind: BpKind) -> Result<BpHandle>;
    fn clear_bp(&mut self, h: BpHandle) -> Result<()>;
    fn set_wp(&mut self, w: &WatchpointSpec) -> Result<WpHandle>;
    fn clear_wp(&mut self, h: WpHandle) -> Result<()>;

    fn download(&mut self, elf_or_bin: &Path, progress: &dyn Fn(Progress))
        -> Result<DownloadStats>;

    fn hss(&mut self) -> Option<&mut dyn HssApi>;
    fn rtt(&mut self) -> Option<&mut dyn RttApi>;
    fn swo(&mut self) -> Option<&mut dyn SwoApi>;

    /// Run a probe-specific command string.
    fn exec(&mut self, _command: &str) -> Result<()> {
        Err(ProbeError::Unsupported("exec".to_owned()))
    }

    /// Change the interface clock, returning the rate actually in effect.
    fn set_speed(&mut self, _khz: u32) -> Result<u32> {
        Err(ProbeError::Unsupported("set_speed".to_owned()))
    }

    /// Latency statistics for each underlying library call.
    fn call_stats(&self) -> CallStats {
        CallStats::default()
    }
}

/// Convenience accessors available on every probe.
pub trait ProbeExt: Probe {
    fn read_u32(&mut self, addr: u32) -> Result<u32> {
        let mut b = [0u8; 4];
        self.read_mem(addr, &mut b, AccessWidth::U32)?;
        Ok(u32::from_le_bytes(b))
    }

    fn write_u32(&mut self, addr: u32, v: u32) -> Result<()> {
        self.write_mem(addr, &v.to_le_bytes(), AccessWidth::U32)
    }

    /// Read-modify-write that only touches the bits in `set` and `clear`.
    fn modify_u32(&mut self, addr: u32, set: u32, clear: u32) -> Result<()> {
        let old = self.read_u32(addr)?;
        let new = (old & !clear) | set;
        if new != old {
            self.write_u32(addr, new)?;
        }
        Ok(())
    }
}

impl<P: Probe + ?Sized> ProbeExt for P {}
