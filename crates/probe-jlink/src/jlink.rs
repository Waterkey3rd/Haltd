use std::collections::{BTreeMap, HashMap};
use std::ffi::{c_char, c_int, CStr, CString};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use probe_api::{
    AccessWidth, BpHandle, BpKind, CallStats, ConnectCfg, DownloadStats, Histogram, HssApi,
    Interface, Probe, ProbeError, ProbeExt, ProbeInfo, Progress, Reg, ResetKind, Result, RttApi,
    SwoApi, TargetInfo, WatchpointSpec, WpAccess, WpHandle,
};

use crate::ffi::{self, Api};

const DWT_CTRL: u32 = 0xE000_1000;
const DWT_CTRL_NUMCOMP_SHIFT: u32 = 28;
const MAX_COMPARATORS: u32 = 16;
const DWT_COMP0: u32 = 0xE000_1020;
const DWT_STRIDE: u32 = 16;
const DWT_MASK_OFFSET: u32 = 4;
const DWT_FUNCTION_OFFSET: u32 = 8;
const DWT_FUNCTION_MASK: u32 = 0xF;
const DWT_FUNCTION_READ: u32 = 5;
const DWT_FUNCTION_WRITE: u32 = 6;
const DWT_FUNCTION_READ_WRITE: u32 = 7;

const CPUID: u32 = 0xE000_ED00;

/// Bit names of `JLINKARM_GetEmuCaps`.
const CAP_NAMES: [&str; 32] = [
    "RESERVED_0",
    "GET_HW_VERSION",
    "WRITE_DCC",
    "ADAPTIVE_CLOCKING",
    "READ_CONFIG",
    "WRITE_CONFIG",
    "TRACE",
    "WRITE_MEM",
    "READ_MEM",
    "SPEED_INFO",
    "EXEC_CODE",
    "GET_MAX_BLOCK_SIZE",
    "GET_HW_INFO",
    "SET_KS_POWER",
    "RESET_STOP_TIMED",
    "RESERVED_15",
    "MEASURE_RTCK_REACT",
    "SELECT_IF",
    "RW_MEM_ARM79",
    "GET_COUNTERS",
    "READ_DCC",
    "GET_CPU_CAPS",
    "EXEC_CPU_CMD",
    "SWO",
    "WRITE_DCC_EX",
    "UPDATE_FIRMWARE_EX",
    "FILE_IO",
    "REGISTER",
    "INDICATORS",
    "TEST_NET_SPEED",
    "RAWTRACE",
    "GET_CAPS_EX",
];

/// Most recent message from the library's error callback. The callbacks carry
/// no context pointer, so this has to be global; the library is single-instance
/// per process anyway.
static LAST_ERROR: Mutex<String> = Mutex::new(String::new());

fn callback_text(text: *const c_char) -> String {
    if text.is_null() {
        return String::new();
    }
    // SAFETY: the library passes a NUL-terminated string valid for the call.
    unsafe { CStr::from_ptr(text) }
        .to_string_lossy()
        .trim_end()
        .to_owned()
}

unsafe extern "C" fn on_log(text: *const c_char) {
    tracing::trace!(target: "jlink", "{}", callback_text(text));
}

unsafe extern "C" fn on_warn(text: *const c_char) {
    tracing::warn!(target: "jlink", "{}", callback_text(text));
}

unsafe extern "C" fn on_error(text: *const c_char) {
    let message = callback_text(text);
    tracing::error!(target: "jlink", "{message}");
    if let Ok(mut last) = LAST_ERROR.lock() {
        *last = message;
    }
}

/// Take the last library error, formatted for appending to a message.
pub(crate) fn take_error_detail() -> String {
    match LAST_ERROR.lock() {
        Ok(mut last) if !last.is_empty() => format!(": {}", std::mem::take(&mut *last)),
        _ => String::new(),
    }
}

pub(crate) fn call_error(func: &'static str, code: i32) -> ProbeError {
    ProbeError::Call {
        func,
        code,
        detail: take_error_detail(),
    }
}

/// Call a library function that may be missing, recording its latency.
macro_rules! dll {
    ($s:expr, $name:ident ( $($arg:expr),* $(,)? )) => {{
        let function = $s
            .api
            .$name
            .ok_or_else(|| probe_api::ProbeError::Unsupported(stringify!($name).to_owned()))?;
        let started = std::time::Instant::now();
        #[allow(unused_unsafe)]
        // SAFETY: arguments are checked by each call site against the
        // documented contract of the library function.
        let result = unsafe { function($($arg),*) };
        $s.record(stringify!($name), started.elapsed());
        result
    }};
}
pub(crate) use dll;

#[derive(Clone, Debug, Default)]
pub struct JLinkOptions {
    /// Path of the library, when it is not in the standard location.
    pub dll: Option<PathBuf>,
    /// Pick a probe by serial number when several are attached.
    pub serial: Option<u32>,
    /// Write the library's own detailed log to this file.
    pub log_file: Option<PathBuf>,
}

pub struct JLink {
    pub(crate) api: Api,
    serial: Option<u32>,
    open: bool,
    connected: bool,
    info: ProbeInfo,
    regs: HashMap<Reg, u32>,
    calls: BTreeMap<&'static str, Histogram>,
}

impl JLink {
    /// Load the library and open the probe, without touching the target.
    pub fn open(options: &JLinkOptions) -> Result<Self> {
        let api = ffi::load(options.dll.as_deref())?;
        let mut jlink = Self {
            api,
            serial: options.serial,
            open: false,
            connected: false,
            info: ProbeInfo::default(),
            regs: HashMap::new(),
            calls: BTreeMap::new(),
        };
        jlink.open_probe()?;
        if let Some(path) = &options.log_file {
            let path = c_path(path)?;
            dll!(jlink, JLINKARM_SetLogFile(path.as_ptr()));
        }
        jlink.info = jlink.read_info()?;
        tracing::info!(
            product = %jlink.info.product,
            serial = jlink.info.serial,
            firmware = %jlink.info.firmware,
            dll = %jlink.info.dll_version,
            "opened probe"
        );
        Ok(jlink)
    }

    pub(crate) fn record(&mut self, name: &'static str, elapsed: Duration) {
        self.calls.entry(name).or_default().record(elapsed);
    }

    fn open_probe(&mut self) -> Result<()> {
        if self.open {
            return Ok(());
        }
        // Optional: the handlers improve diagnostics but opening works without.
        if self.api.JLINKARM_SetErrorOutHandler.is_some() {
            dll!(self, JLINKARM_SetErrorOutHandler(Some(on_error)));
        }
        if self.api.JLINKARM_SetWarnOutHandler.is_some() {
            dll!(self, JLINKARM_SetWarnOutHandler(Some(on_warn)));
        }
        if let Some(serial) = self.serial {
            let code = dll!(self, JLINKARM_EMU_SelectByUSBSN(serial));
            if code < 0 {
                return Err(ProbeError::Other(format!(
                    "no J-Link with serial number {serial} found"
                )));
            }
        }
        let error = dll!(self, JLINKARM_OpenEx(Some(on_log), Some(on_error)));
        if !error.is_null() {
            return Err(ProbeError::Other(format!(
                "cannot open J-Link: {}",
                callback_text(error)
            )));
        }
        self.open = true;
        Ok(())
    }

    fn read_info(&mut self) -> Result<ProbeInfo> {
        let mut info = ProbeInfo {
            missing_symbols: self.api.missing.iter().map(|s| (*s).to_owned()).collect(),
            ..ProbeInfo::default()
        };
        let mut buf = [0 as c_char; 256];
        if self.api.JLINKARM_EMU_GetProductName.is_some() {
            dll!(self, JLINKARM_EMU_GetProductName(buf.as_mut_ptr(), 256));
            info.product = c_buffer(&buf);
        }
        if self.api.JLINKARM_GetFirmwareString.is_some() {
            dll!(self, JLINKARM_GetFirmwareString(buf.as_mut_ptr(), 256));
            info.firmware = c_buffer(&buf);
        }
        if self.api.JLINKARM_GetSN.is_some() {
            info.serial = dll!(self, JLINKARM_GetSN()) as u32;
        }
        if self.api.JLINKARM_GetHardwareVersion.is_some() {
            let v = dll!(self, JLINKARM_GetHardwareVersion());
            info.hardware_version = format!("{}.{:02}", v / 10000, (v / 100) % 100);
        }
        if self.api.JLINKARM_GetDLLVersion.is_some() {
            info.dll_version = format_dll_version(dll!(self, JLINKARM_GetDLLVersion()));
        }
        if self.api.JLINKARM_GetEmuCaps.is_some() {
            info.caps = dll!(self, JLINKARM_GetEmuCaps());
            info.cap_names = cap_names(info.caps);
        }
        if self.api.JLINKARM_GetEmuCapsEx.is_some() {
            let mut caps = [0u8; 32];
            dll!(self, JLINKARM_GetEmuCapsEx(caps.as_mut_ptr(), 32));
            info.caps_ex = caps.to_vec();
        }
        Ok(info)
    }

    fn build_register_map(&mut self) -> Result<()> {
        let mut ids = [0u32; 256];
        let count = dll!(self, JLINKARM_GetRegisterList(ids.as_mut_ptr(), 256));
        self.regs.clear();
        for &id in ids.iter().take(count.max(0) as usize) {
            let name = dll!(self, JLINKARM_GetRegisterName(id));
            if let Some(reg) = parse_register_name(&callback_text(name)) {
                self.regs.insert(reg, id);
            }
        }
        if self.regs.is_empty() {
            return Err(ProbeError::Other(
                "the J-Link library reported no usable core registers".to_owned(),
            ));
        }
        Ok(())
    }

    fn reg_index(&self, reg: Reg) -> Result<u32> {
        self.regs
            .get(&reg)
            .copied()
            .ok_or_else(|| ProbeError::Unsupported(format!("register {reg:?} on this target")))
    }

    fn ensure_connected(&self) -> Result<()> {
        if self.connected {
            Ok(())
        } else {
            Err(ProbeError::NotConnected)
        }
    }
}

impl Drop for JLink {
    fn drop(&mut self) {
        if self.open {
            if let Some(close) = self.api.JLINKARM_Close {
                // SAFETY: the probe is open and no other thread uses this object.
                unsafe { close() };
            }
        }
    }
}

impl Probe for JLink {
    fn info(&self) -> ProbeInfo {
        self.info.clone()
    }

    fn connect(&mut self, cfg: &ConnectCfg) -> Result<TargetInfo> {
        self.open_probe()?;
        self.exec(&format!("Device = {}", cfg.device))?;
        let interface: c_int = match cfg.interface {
            Interface::Jtag => 0,
            Interface::Swd => 1,
        };
        dll!(self, JLINKARM_TIF_Select(interface));
        dll!(self, JLINKARM_SetSpeed(cfg.speed_khz));
        let code = dll!(self, JLINKARM_Connect());
        if code < 0 {
            return Err(call_error("JLINKARM_Connect", code));
        }
        self.connected = true;
        self.build_register_map()?;
        let speed_khz = u32::from(dll!(self, JLINKARM_GetSpeed()));
        let cpuid = self.read_u32(CPUID)?;
        let hw_breakpoints = dll!(self, JLINKARM_GetNumBPUnits(ffi::BP_HW)).max(0) as u32;
        let halted = self.is_halted()?;
        Ok(TargetInfo {
            device: cfg.device.clone(),
            speed_khz,
            cpuid,
            hw_breakpoints,
            halted,
        })
    }

    fn disconnect(&mut self) -> Result<()> {
        if self.open {
            dll!(self, JLINKARM_Close());
        }
        self.open = false;
        self.connected = false;
        Ok(())
    }

    fn halt(&mut self) -> Result<()> {
        self.ensure_connected()?;
        let code = dll!(self, JLINKARM_Halt());
        if code != 0 {
            return Err(call_error("JLINKARM_Halt", code.into()));
        }
        Ok(())
    }

    fn go(&mut self) -> Result<()> {
        self.ensure_connected()?;
        dll!(self, JLINKARM_Go());
        Ok(())
    }

    fn step_instr(&mut self) -> Result<()> {
        self.ensure_connected()?;
        let code = dll!(self, JLINKARM_Step());
        if code != 0 {
            return Err(call_error("JLINKARM_Step", code.into()));
        }
        Ok(())
    }

    fn reset(&mut self, kind: ResetKind, halt_after: bool) -> Result<()> {
        self.ensure_connected()?;
        let strategy = match kind {
            ResetKind::Normal => 0,
            ResetKind::Core => 1,
            ResetKind::Pin => 2,
        };
        dll!(self, JLINKARM_SetResetType(strategy));
        let code = dll!(self, JLINKARM_Reset());
        if code < 0 {
            return Err(call_error("JLINKARM_Reset", code));
        }
        if !halt_after {
            dll!(self, JLINKARM_Go());
        }
        Ok(())
    }

    fn is_halted(&mut self) -> Result<bool> {
        self.ensure_connected()?;
        let code = dll!(self, JLINKARM_IsHalted());
        if code < 0 {
            return Err(call_error("JLINKARM_IsHalted", code.into()));
        }
        Ok(code > 0)
    }

    fn read_regs(&mut self, ids: &[Reg], out: &mut [u32]) -> Result<()> {
        self.ensure_connected()?;
        assert_eq!(ids.len(), out.len());
        let indices = ids
            .iter()
            .map(|&reg| self.reg_index(reg))
            .collect::<Result<Vec<_>>>()?;
        let mut status = vec![0u8; ids.len()];
        let code = dll!(
            self,
            JLINKARM_ReadRegs(
                indices.as_ptr(),
                out.as_mut_ptr(),
                status.as_mut_ptr(),
                ids.len() as u32,
            )
        );
        if code < 0 || status.iter().any(|&s| s != 0) {
            return Err(call_error("JLINKARM_ReadRegs", code));
        }
        Ok(())
    }

    fn write_reg(&mut self, id: Reg, v: u32) -> Result<()> {
        self.ensure_connected()?;
        let index = self.reg_index(id)?;
        let code = dll!(self, JLINKARM_WriteReg(index, v));
        if code != 0 {
            return Err(call_error("JLINKARM_WriteReg", code.into()));
        }
        Ok(())
    }

    fn read_mem(&mut self, addr: u32, buf: &mut [u8], width: AccessWidth) -> Result<()> {
        self.ensure_connected()?;
        if buf.is_empty() {
            return Ok(());
        }
        let length = buf.len() as u32;
        let code = dll!(
            self,
            JLINKARM_ReadMemEx(addr, length, buf.as_mut_ptr(), width.bytes())
        );
        check_transfer(addr, buf.len(), width, code)
    }

    fn write_mem(&mut self, addr: u32, data: &[u8], width: AccessWidth) -> Result<()> {
        self.ensure_connected()?;
        if data.is_empty() {
            return Ok(());
        }
        let length = data.len() as u32;
        let code = dll!(
            self,
            JLINKARM_WriteMemEx(addr, length, data.as_ptr(), width.bytes())
        );
        check_transfer(addr, data.len(), width, code)
    }

    fn set_bp(&mut self, addr: u32, kind: BpKind) -> Result<BpHandle> {
        self.ensure_connected()?;
        let flags = ffi::BP_THUMB
            | match kind {
                BpKind::Hw => ffi::BP_HW,
                BpKind::Flash => ffi::BP_SW_FLASH,
                BpKind::Any => ffi::BP_ANY,
            };
        let handle = dll!(self, JLINKARM_SetBPEx(addr, flags));
        if handle <= 0 {
            return Err(call_error("JLINKARM_SetBPEx", handle));
        }
        Ok(BpHandle(handle as u32))
    }

    fn clear_bp(&mut self, h: BpHandle) -> Result<()> {
        self.ensure_connected()?;
        let code = dll!(self, JLINKARM_ClrBPEx(h.0 as c_int));
        if code != 0 {
            return Err(call_error("JLINKARM_ClrBPEx", code));
        }
        Ok(())
    }

    /// The comparators are programmed directly. Going through the library's
    /// JLINKARM_SetWP makes it rewrite DEMCR on every start, which silently
    /// turns off vector catch.
    fn set_wp(&mut self, w: &WatchpointSpec) -> Result<WpHandle> {
        self.ensure_connected()?;
        let function = match w.access {
            WpAccess::Read => DWT_FUNCTION_READ,
            WpAccess::Write => DWT_FUNCTION_WRITE,
            WpAccess::ReadWrite => DWT_FUNCTION_READ_WRITE,
        };
        let ignored_bits = w.addr_mask.count_ones();
        let comparators = (self.read_u32(DWT_CTRL)? >> DWT_CTRL_NUMCOMP_SHIFT).min(MAX_COMPARATORS);
        for index in 0..comparators {
            let base = DWT_COMP0 + index * DWT_STRIDE;
            if self.read_u32(base + DWT_FUNCTION_OFFSET)? & DWT_FUNCTION_MASK != 0 {
                continue;
            }
            self.write_u32(base, w.addr & !w.addr_mask)?;
            self.write_u32(base + DWT_MASK_OFFSET, ignored_bits)?;
            // Implementations limit how many address bits can be ignored.
            if self.read_u32(base + DWT_MASK_OFFSET)? != ignored_bits {
                return Err(ProbeError::Other(format!(
                    "this core cannot watch {} bytes at once",
                    u64::from(w.addr_mask) + 1
                )));
            }
            self.write_u32(base + DWT_FUNCTION_OFFSET, function)?;
            return Ok(WpHandle(index + 1));
        }
        Err(ProbeError::Other(
            "every DWT comparator is already in use".to_owned(),
        ))
    }

    fn clear_wp(&mut self, h: WpHandle) -> Result<()> {
        self.ensure_connected()?;
        let index =
            h.0.checked_sub(1)
                .ok_or_else(|| ProbeError::Other("invalid watchpoint handle".to_owned()))?;
        self.write_u32(DWT_COMP0 + index * DWT_STRIDE + DWT_FUNCTION_OFFSET, 0)
    }

    fn download(
        &mut self,
        elf_or_bin: &Path,
        progress: &dyn Fn(Progress),
    ) -> Result<DownloadStats> {
        self.ensure_connected()?;
        crate::flash::download(self, elf_or_bin, progress)
    }

    fn hss(&mut self) -> Option<&mut dyn HssApi> {
        self.api.JLINK_HSS_Start.is_some().then_some(self as _)
    }

    fn rtt(&mut self) -> Option<&mut dyn RttApi> {
        self.api
            .JLINK_RTTERMINAL_Control
            .is_some()
            .then_some(self as _)
    }

    fn swo(&mut self) -> Option<&mut dyn SwoApi> {
        None
    }

    fn exec(&mut self, command: &str) -> Result<()> {
        let c_command = CString::new(command)
            .map_err(|_| ProbeError::Other("command contains a NUL byte".to_owned()))?;
        let mut error = [0 as c_char; 512];
        dll!(
            self,
            JLINKARM_ExecCommand(c_command.as_ptr(), error.as_mut_ptr(), 512)
        );
        let message = c_buffer(&error);
        if message.is_empty() {
            Ok(())
        } else {
            Err(ProbeError::Other(format!("{command:?}: {message}")))
        }
    }

    fn set_speed(&mut self, khz: u32) -> Result<u32> {
        dll!(self, JLINKARM_SetSpeed(khz));
        Ok(u32::from(dll!(self, JLINKARM_GetSpeed())))
    }

    fn call_stats(&self) -> CallStats {
        CallStats {
            calls: self
                .calls
                .iter()
                .map(|(name, histogram)| ((*name).to_owned(), histogram.summary()))
                .collect(),
        }
    }
}

pub(crate) fn c_buffer(buf: &[c_char]) -> String {
    // SAFETY: c_char and u8 have the same layout.
    let bytes = unsafe { std::slice::from_raw_parts(buf.as_ptr().cast::<u8>(), buf.len()) };
    CStr::from_bytes_until_nul(bytes)
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default()
}

pub(crate) fn c_path(path: &Path) -> Result<CString> {
    use std::os::unix::ffi::OsStrExt;
    CString::new(path.as_os_str().as_bytes())
        .map_err(|_| ProbeError::Other(format!("path contains a NUL byte: {}", path.display())))
}

fn check_transfer(addr: u32, len: usize, width: AccessWidth, code: c_int) -> Result<()> {
    // Depending on the path taken, the library reports either bytes or items.
    let items = len as i64 / i64::from(width.bytes().max(1));
    if i64::from(code) == len as i64 || i64::from(code) == items {
        Ok(())
    } else {
        Err(ProbeError::Memory {
            addr,
            len,
            detail: format!(" (returned {code}){}", take_error_detail()),
        })
    }
}

fn cap_names(caps: u32) -> Vec<String> {
    CAP_NAMES
        .iter()
        .enumerate()
        .filter(|(bit, _)| caps & (1 << bit) != 0)
        .map(|(_, name)| (*name).to_owned())
        .collect()
}

fn format_dll_version(version: u32) -> String {
    let revision = version % 100;
    let mut text = format!("{}.{:02}", version / 10000, (version / 100) % 100);
    if (1..=26).contains(&revision) {
        text.push((b'a' + revision as u8 - 1) as char);
    }
    text
}

/// Map a name from `JLINKARM_GetRegisterName`, such as "R13 (SP)", to a [`Reg`].
fn parse_register_name(name: &str) -> Option<Reg> {
    let name = name.split([' ', '(']).next()?.to_ascii_uppercase();
    let numbered = |prefix: &str, limit: u8| -> Option<u8> {
        let n = name.strip_prefix(prefix)?.parse::<u8>().ok()?;
        (n < limit).then_some(n)
    };
    match name.as_str() {
        "SP" => Some(Reg::SP),
        "LR" => Some(Reg::LR),
        "PC" => Some(Reg::PC),
        "XPSR" => Some(Reg::Xpsr),
        "MSP" => Some(Reg::Msp),
        "PSP" => Some(Reg::Psp),
        "PRIMASK" => Some(Reg::Primask),
        "BASEPRI" => Some(Reg::Basepri),
        "FAULTMASK" => Some(Reg::Faultmask),
        "CONTROL" => Some(Reg::Control),
        "FPSCR" => Some(Reg::Fpscr),
        _ => numbered("R", 16)
            .map(Reg::R)
            .or_else(|| numbered("FPS", 32).map(Reg::S))
            .or_else(|| numbered("S", 32).map(Reg::S)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_names_parse() {
        assert_eq!(parse_register_name("R0"), Some(Reg::R(0)));
        assert_eq!(parse_register_name("R13 (SP)"), Some(Reg::SP));
        assert_eq!(parse_register_name("R15 (PC)"), Some(Reg::PC));
        assert_eq!(parse_register_name("XPSR"), Some(Reg::Xpsr));
        assert_eq!(parse_register_name("FPS31"), Some(Reg::S(31)));
        assert_eq!(parse_register_name("FPSCR"), Some(Reg::Fpscr));
        assert_eq!(parse_register_name("CycleCnt"), None);
        assert_eq!(parse_register_name("R16"), None);
    }

    #[test]
    fn dll_version_formats_like_segger() {
        assert_eq!(format_dll_version(98000), "9.80");
        assert_eq!(format_dll_version(98002), "9.80b");
    }

    #[test]
    fn transfer_accepts_bytes_or_items_only() {
        assert!(check_transfer(0, 8, AccessWidth::U32, 8).is_ok());
        assert!(check_transfer(0, 8, AccessWidth::U32, 2).is_ok());
        assert!(check_transfer(0, 8, AccessWidth::U32, 1).is_err());
        assert!(check_transfer(0, 8, AccessWidth::U32, -1).is_err());
        assert!(check_transfer(0, 8, AccessWidth::Auto, 8).is_ok());
    }

    #[test]
    fn capabilities_decode_by_bit() {
        assert_eq!(cap_names(1 << 23), vec!["SWO".to_owned()]);
    }
}
