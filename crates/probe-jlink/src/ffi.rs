//! Hand-written bindings for the J-Link DLL.
//!
//! Every symbol is optional: a missing one disables only the feature using it
//! and is reported through `ProbeInfo::missing_symbols`.
//!
//! Sources for the signatures: the HSS functions and structure sizes were
//! checked against the disassembly of libjlinkarm.so V9.80. The rest follow
//! pylink.
#![allow(non_snake_case)]

use std::ffi::{c_char, c_int, c_void};
use std::path::{Path, PathBuf};

use libloading::Library;
use probe_api::{ProbeError, Result};

pub type LogCb = unsafe extern "C" fn(*const c_char);
pub type ProgressCb = unsafe extern "C" fn(*const c_char, *const c_char, c_int);

#[repr(C)]
#[derive(Default)]
pub struct HssCapsRaw {
    pub words: [u32; 8],
}

#[repr(C)]
pub struct HssBlockDesc {
    pub addr: u32,
    pub num_bytes: u32,
    pub flags: u32,
    pub reserved: u32,
}

#[repr(C)]
#[derive(Default)]
pub struct RttStart {
    pub control_block: u32,
    pub reserved: [u32; 3],
}

#[repr(C)]
pub struct RttBufDesc {
    pub index: i32,
    pub direction: u32,
    pub name: [c_char; 32],
    pub size: u32,
    pub flags: u32,
}

#[repr(C)]
#[derive(Default)]
pub struct RttStatusRaw {
    pub bytes_transferred: u32,
    pub bytes_read: u32,
    pub host_overflows: i32,
    pub running: i32,
    pub up_buffers: i32,
    pub down_buffers: i32,
    pub reserved: [u32; 2],
}

pub const RTT_CMD_START: u32 = 0;
pub const RTT_CMD_STOP: u32 = 1;
pub const RTT_CMD_GETDESC: u32 = 2;
pub const RTT_CMD_GETNUMBUF: u32 = 3;
pub const RTT_CMD_GETSTAT: u32 = 4;

pub const BP_THUMB: u32 = 2;
pub const BP_SW_FLASH: u32 = 1 << 5;
pub const BP_HW: u32 = 0xFFFF_FF00;
pub const BP_ANY: u32 = 0xFFFF_FFF0;

macro_rules! jlink_api {
    ($( $name:ident : fn($($arg:ty),* $(,)?) $(-> $ret:ty)? ;)*) => {
        pub struct Api {
            _lib: Library,
            pub missing: Vec<&'static str>,
            $(pub $name: Option<unsafe extern "C" fn($($arg),*) $(-> $ret)?>,)*
        }

        impl Api {
            fn bind(lib: Library) -> Self {
                let mut missing = Vec::new();
                $(
                    // SAFETY: the type matches the declaration in this table,
                    // and `_lib` keeps the code mapped as long as the pointer.
                    let $name = unsafe {
                        lib.get::<unsafe extern "C" fn($($arg),*) $(-> $ret)?>(
                            concat!(stringify!($name), "\0").as_bytes(),
                        )
                    }
                    .map(|symbol| *symbol)
                    .ok();
                    if $name.is_none() {
                        missing.push(stringify!($name));
                    }
                )*
                Self { _lib: lib, missing, $($name,)* }
            }
        }
    };
}

jlink_api! {
    // Connection.
    JLINKARM_OpenEx: fn(Option<LogCb>, Option<LogCb>) -> *const c_char;
    JLINKARM_Close: fn();
    JLINKARM_ExecCommand: fn(*const c_char, *mut c_char, c_int) -> c_int;
    JLINKARM_TIF_Select: fn(c_int) -> c_int;
    JLINKARM_SetSpeed: fn(u32);
    JLINKARM_GetSpeed: fn() -> u16;
    JLINKARM_Connect: fn() -> c_int;
    JLINKARM_EMU_SelectByUSBSN: fn(u32) -> c_int;
    // Identification.
    JLINKARM_GetSN: fn() -> c_int;
    JLINKARM_GetFirmwareString: fn(*mut c_char, c_int);
    JLINKARM_GetDLLVersion: fn() -> u32;
    JLINKARM_GetHardwareVersion: fn() -> u32;
    JLINKARM_EMU_GetProductName: fn(*mut c_char, c_int);
    JLINKARM_GetEmuCaps: fn() -> u32;
    JLINKARM_GetEmuCapsEx: fn(*mut u8, c_int);
    // Logging.
    JLINKARM_SetLogFile: fn(*const c_char);
    JLINKARM_SetErrorOutHandler: fn(Option<LogCb>);
    JLINKARM_SetWarnOutHandler: fn(Option<LogCb>);
    // Run control.
    JLINKARM_Halt: fn() -> i8;
    JLINKARM_IsHalted: fn() -> i8;
    JLINKARM_Go: fn();
    JLINKARM_Step: fn() -> i8;
    JLINKARM_Reset: fn() -> c_int;
    JLINKARM_SetResetType: fn(c_int) -> c_int;
    // Registers and memory.
    JLINKARM_ReadRegs: fn(*const u32, *mut u32, *mut u8, u32) -> c_int;
    JLINKARM_WriteReg: fn(u32, u32) -> i8;
    JLINKARM_GetRegisterList: fn(*mut u32, c_int) -> c_int;
    JLINKARM_GetRegisterName: fn(u32) -> *const c_char;
    JLINKARM_ReadMemEx: fn(u32, u32, *mut u8, u32) -> c_int;
    JLINKARM_WriteMemEx: fn(u32, u32, *const u8, u32) -> c_int;
    // Breakpoints.
    JLINKARM_SetBPEx: fn(u32, u32) -> c_int;
    JLINKARM_ClrBPEx: fn(c_int) -> c_int;
    JLINKARM_GetNumBPUnits: fn(u32) -> c_int;
    // Flash.
    JLINK_DownloadFile: fn(*const c_char, u32) -> c_int;
    JLINK_SetFlashProgProgressCallback: fn(Option<ProgressCb>);
    // High speed sampling.
    JLINK_HSS_GetCaps: fn(*mut HssCapsRaw) -> c_int;
    JLINK_HSS_Start: fn(*const HssBlockDesc, c_int, c_int, c_int) -> c_int;
    JLINK_HSS_Read: fn(*mut u8, u32) -> c_int;
    JLINK_HSS_Stop: fn() -> c_int;
    // RTT.
    JLINK_RTTERMINAL_Control: fn(u32, *mut c_void) -> c_int;
    JLINK_RTTERMINAL_Read: fn(u32, *mut u8, u32) -> c_int;
    JLINK_RTTERMINAL_Write: fn(u32, *const u8, u32) -> c_int;
}

#[cfg(not(windows))]
fn default_candidates() -> Vec<PathBuf> {
    vec![
        PathBuf::from("/opt/SEGGER/JLink/libjlinkarm.so"),
        PathBuf::from("libjlinkarm.so"),
    ]
}

/// Where the SEGGER installer recorded itself, then the system search path.
#[cfg(windows)]
fn default_candidates() -> Vec<PathBuf> {
    use std::os::windows::ffi::{OsStrExt, OsStringExt};
    use windows_sys::Win32::Foundation::ERROR_SUCCESS;
    use windows_sys::Win32::System::Registry::{
        RegGetValueW, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ,
    };

    const LIBRARY: &str = "JLink_x64.dll";
    let wide = |text: &str| -> Vec<u16> {
        std::ffi::OsStr::new(text)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect()
    };
    let key = wide(r"Software\SEGGER\J-Link");
    let value = wide("InstallPath");
    let mut candidates = Vec::new();
    for root in [HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE] {
        let mut buffer = [0u16; 1024];
        let mut size = size_of_val(&buffer) as u32;
        // SAFETY: both names are NUL-terminated and `size` is the buffer's size in bytes.
        let status = unsafe {
            RegGetValueW(
                root,
                key.as_ptr(),
                value.as_ptr(),
                RRF_RT_REG_SZ,
                std::ptr::null_mut(),
                buffer.as_mut_ptr().cast(),
                &mut size,
            )
        };
        if status == ERROR_SUCCESS {
            // `size` counts the terminating NUL.
            let length = (size as usize / 2).saturating_sub(1);
            let directory = std::ffi::OsString::from_wide(&buffer[..length]);
            candidates.push(PathBuf::from(directory).join(LIBRARY));
        }
    }
    candidates.push(PathBuf::from(LIBRARY));
    candidates
}

/// Load the DLL: the explicit path, or else the standard install location and
/// then the system search path.
pub fn load(explicit: Option<&Path>) -> Result<Api> {
    let candidates: Vec<PathBuf> = match explicit {
        Some(path) => vec![path.to_owned()],
        None => default_candidates(),
    };
    let mut errors = Vec::new();
    for path in &candidates {
        // SAFETY: loading the vendor library runs its initializers, which is
        // what every J-Link tool does; we load only the configured paths.
        match unsafe { Library::new(path) } {
            Ok(lib) => {
                tracing::debug!(path = %path.display(), "loaded J-Link library");
                return Ok(Api::bind(lib));
            }
            Err(e) => errors.push(format!("{}: {e}", path.display())),
        }
    }
    Err(ProbeError::Load(errors.join("; ")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::mem::size_of;

    /// Sizes taken from the library's own marshalling code.
    #[test]
    fn struct_sizes_match_library() {
        assert_eq!(size_of::<HssCapsRaw>(), 0x20);
        assert_eq!(size_of::<HssBlockDesc>(), 0x10);
        assert_eq!(size_of::<RttStart>(), 16);
        assert_eq!(size_of::<RttBufDesc>(), 48);
        assert_eq!(size_of::<RttStatusRaw>(), 32);
    }
}
