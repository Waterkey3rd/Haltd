//! Per-family debug configuration and the post-connect setup (DESIGN §5.2.4).

use std::collections::BTreeMap;

use serde::Deserialize;

use crate::{Probe, ProbeError, ProbeExt, Result};

const DEMCR: u32 = 0xE000_EDFC;
const DEMCR_TRCENA: u32 = 1 << 24;
const DEMCR_VC_MMERR: u32 = 1 << 4;
const DEMCR_VC_NOCPERR: u32 = 1 << 5;
const DEMCR_VC_CHKERR: u32 = 1 << 6;
const DEMCR_VC_STATERR: u32 = 1 << 7;
const DEMCR_VC_BUSERR: u32 = 1 << 8;
const DEMCR_VC_INTERR: u32 = 1 << 9;
const DEMCR_VC_HARDERR: u32 = 1 << 10;

/// DEMCR vector-catch bits for each fault an editor can ask to stop on.
pub const VC_HARDFAULT: u32 = DEMCR_VC_HARDERR | DEMCR_VC_INTERR;
pub const VC_BUSFAULT: u32 = DEMCR_VC_BUSERR;
pub const VC_MEMMANAGE: u32 = DEMCR_VC_MMERR;
pub const VC_USAGEFAULT: u32 = DEMCR_VC_STATERR | DEMCR_VC_CHKERR | DEMCR_VC_NOCPERR;
pub const VC_ALL_FAULTS: u32 = VC_HARDFAULT | VC_BUSFAULT | VC_MEMMANAGE | VC_USAGEFAULT;

const DWT_CTRL: u32 = 0xE000_1000;
const DWT_CTRL_CYCCNTENA: u32 = 1;

const STM32H7_YAML: &str = include_str!("../../../devices/stm32h7.yaml");
const STM32F1_YAML: &str = include_str!("../../../devices/stm32f1.yaml");

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceProfile {
    pub family: String,
    pub memory: Vec<MemoryRegion>,
    pub flash_sector_size: u32,
    #[serde(default)]
    pub post_connect: Vec<RegisterWrite>,
    #[serde(default)]
    pub freeze: BTreeMap<String, RegisterBit>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct MemoryRegion {
    pub name: String,
    pub start: u32,
    pub size: u32,
    pub kind: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct RegisterWrite {
    pub name: String,
    pub addr: u32,
    pub set: u32,
}

#[derive(Clone, Copy, Debug, Deserialize)]
pub struct RegisterBit {
    pub addr: u32,
    pub bit: u32,
}

impl DeviceProfile {
    /// Load a profile built into the binary.
    pub fn builtin(family: &str) -> Result<Self> {
        let yaml = match family {
            "stm32h7" => STM32H7_YAML,
            "stm32f1" => STM32F1_YAML,
            other => {
                return Err(ProbeError::Other(format!(
                    "no built-in device profile for {other:?}"
                )))
            }
        };
        serde_yaml::from_str(yaml)
            .map_err(|e| ProbeError::Other(format!("invalid profile {family}: {e}")))
    }
}

#[derive(Clone, Debug, Default)]
pub struct PostConnectOptions {
    /// `VC_*` bits: which fault handlers halt on entry. The others are cleared.
    pub vector_catch: u32,
    /// Names from [`DeviceProfile::freeze`] to stop while the core is halted.
    pub freeze_on_halt: Vec<String>,
}

/// Turn on the DWT cycle counter without resetting it.
pub fn enable_cycle_counter(probe: &mut dyn Probe) -> Result<()> {
    probe.modify_u32(DEMCR, DEMCR_TRCENA, 0)?;
    probe.modify_u32(DWT_CTRL, DWT_CTRL_CYCCNTENA, 0)
}

/// Apply the debug setup a session needs. Every write is a read-modify-write
/// of only the listed bits, so firmware-owned bits survive.
pub fn apply_post_connect(
    probe: &mut dyn Probe,
    profile: &DeviceProfile,
    options: &PostConnectOptions,
) -> Result<()> {
    enable_cycle_counter(probe)?;
    probe.modify_u32(
        DEMCR,
        options.vector_catch,
        VC_ALL_FAULTS & !options.vector_catch,
    )?;
    for write in &profile.post_connect {
        probe.modify_u32(write.addr, write.set, 0)?;
    }
    for name in &options.freeze_on_halt {
        let bit = profile.freeze.get(name).ok_or_else(|| {
            ProbeError::Other(format!(
                "{} has no peripheral named {name:?} to freeze",
                profile.family
            ))
        })?;
        probe.modify_u32(bit.addr, 1 << bit.bit, 0)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_h7_profile_parses() {
        let p = DeviceProfile::builtin("stm32h7").unwrap();
        assert_eq!(p.flash_sector_size, 128 * 1024);
        assert_eq!(p.post_connect[0].set, 0x0060_0187);
        assert_eq!(p.freeze["iwdg1"].addr, 0x5C00_1054);
        assert_eq!(p.freeze["iwdg1"].bit, 18);
        assert_eq!(p.freeze["wwdg1"].bit, 6);
    }

    #[test]
    fn unknown_family_is_an_error() {
        assert!(DeviceProfile::builtin("stm32xx").is_err());
    }
}
