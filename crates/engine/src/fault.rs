//! Explaining Cortex-M faults (DESIGN §5.5.5).

use probe_api::{Probe, ProbeExt};

const CFSR: u32 = 0xE000_ED28;
const HFSR: u32 = 0xE000_ED2C;
const MMFAR: u32 = 0xE000_ED34;
const BFAR: u32 = 0xE000_ED38;
const AFSR: u32 = 0xE000_ED3C;

const MMARVALID: u32 = 1 << 7;
const BFARVALID: u32 = 1 << 15;
const IMPRECISERR: u32 = 1 << 10;

/// Register a bit belongs to, the bit, its name and what it means.
const CFSR_BITS: [(&str, u32, &str, &str); 16] = [
    (
        "MMFSR",
        0,
        "IACCVIOL",
        "instruction fetch from a region that does not allow execution",
    ),
    (
        "MMFSR",
        1,
        "DACCVIOL",
        "data access violates the memory protection rules",
    ),
    (
        "MMFSR",
        3,
        "MUNSTKERR",
        "memory protection fault while unstacking on exception return",
    ),
    (
        "MMFSR",
        4,
        "MSTKERR",
        "memory protection fault while stacking on exception entry",
    ),
    (
        "MMFSR",
        5,
        "MLSPERR",
        "memory protection fault during lazy floating-point state saving",
    ),
    ("BFSR", 8, "IBUSERR", "bus error on instruction fetch"),
    ("BFSR", 9, "PRECISERR", "precise data bus error"),
    ("BFSR", 10, "IMPRECISERR", "imprecise data bus error"),
    (
        "BFSR",
        11,
        "UNSTKERR",
        "bus error while unstacking on exception return",
    ),
    (
        "BFSR",
        12,
        "STKERR",
        "bus error while stacking on exception entry, often a stack overflow",
    ),
    (
        "BFSR",
        13,
        "LSPERR",
        "bus error during lazy floating-point state saving",
    ),
    ("UFSR", 16, "UNDEFINSTR", "undefined instruction"),
    (
        "UFSR",
        17,
        "INVSTATE",
        "invalid execution state, such as branching to an even address",
    ),
    (
        "UFSR",
        18,
        "INVPC",
        "invalid EXC_RETURN value on exception return",
    ),
    (
        "UFSR",
        24,
        "UNALIGNED",
        "unaligned memory access while unaligned trapping is on",
    ),
    (
        "UFSR",
        25,
        "DIVBYZERO",
        "integer division by zero while the trap is on",
    ),
];

const NOCP_BIT: u32 = 19;
const HFSR_VECTTBL: u32 = 1 << 1;
const HFSR_FORCED: u32 = 1 << 30;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FaultRegisters {
    pub cfsr: u32,
    pub hfsr: u32,
    pub mmfar: u32,
    pub bfar: u32,
    pub afsr: u32,
}

impl FaultRegisters {
    pub fn read(probe: &mut dyn Probe) -> probe_api::Result<Self> {
        Ok(Self {
            cfsr: probe.read_u32(CFSR)?,
            hfsr: probe.read_u32(HFSR)?,
            mmfar: probe.read_u32(MMFAR)?,
            bfar: probe.read_u32(BFAR)?,
            afsr: probe.read_u32(AFSR)?,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FaultCause {
    pub register: &'static str,
    pub name: &'static str,
    pub meaning: &'static str,
}

/// Name of a fault exception, from IPSR.
pub fn exception_name(number: u32) -> String {
    match number {
        3 => "HardFault".to_owned(),
        4 => "MemManage".to_owned(),
        5 => "BusFault".to_owned(),
        6 => "UsageFault".to_owned(),
        other => format!("exception {other}"),
    }
}

pub fn decode_causes(registers: &FaultRegisters) -> Vec<FaultCause> {
    let mut causes = Vec::new();
    if registers.hfsr & HFSR_VECTTBL != 0 {
        causes.push(FaultCause {
            register: "HFSR",
            name: "VECTTBL",
            meaning: "bus error reading the vector table",
        });
    }
    for (register, bit, name, meaning) in CFSR_BITS {
        if registers.cfsr & (1 << bit) != 0 {
            causes.push(FaultCause {
                register,
                name,
                meaning,
            });
        }
    }
    if registers.cfsr & (1 << NOCP_BIT) != 0 {
        causes.push(FaultCause {
            register: "UFSR",
            name: "NOCP",
            meaning: "coprocessor instruction with the coprocessor disabled, usually the FPU",
        });
    }
    if registers.hfsr & HFSR_FORCED != 0 {
        causes.push(FaultCause {
            register: "HFSR",
            name: "FORCED",
            meaning: "escalated to HardFault because the configurable fault was disabled or could not run",
        });
    }
    causes
}

/// The memory address the hardware reports, if it is valid.
pub fn fault_address(registers: &FaultRegisters) -> Option<(&'static str, u32)> {
    if registers.cfsr & BFARVALID != 0 {
        Some(("BFAR", registers.bfar))
    } else if registers.cfsr & MMARVALID != 0 {
        Some(("MMFAR", registers.mmfar))
    } else {
        None
    }
}

pub fn is_imprecise(registers: &FaultRegisters) -> bool {
    registers.cfsr & IMPRECISERR != 0
}

/// A one-line explanation, naming the most specific cause first.
pub fn summarize(exception: u32, registers: &FaultRegisters) -> String {
    let causes = decode_causes(registers);
    // FORCED only says how it got here; the configurable cause says why.
    let main = causes
        .iter()
        .find(|cause| cause.name != "FORCED")
        .or(causes.first());
    let mut text = exception_name(exception);
    if let Some(cause) = main {
        text.push_str(&format!(": {} ({})", cause.meaning, cause.name));
    }
    if let Some((register, address)) = fault_address(registers) {
        text.push_str(&format!(" at {address:#010x} [{register}]"));
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    fn registers(cfsr: u32, hfsr: u32) -> FaultRegisters {
        FaultRegisters {
            cfsr,
            hfsr,
            ..FaultRegisters::default()
        }
    }

    fn names(registers: &FaultRegisters) -> Vec<&'static str> {
        decode_causes(registers)
            .into_iter()
            .map(|cause| cause.name)
            .collect()
    }

    #[test]
    fn each_status_bit_is_named() {
        assert_eq!(names(&registers(1 << 16, 0)), ["UNDEFINSTR"]);
        assert_eq!(names(&registers(1 << 25, 0)), ["DIVBYZERO"]);
        assert_eq!(names(&registers(1 << 19, 0)), ["NOCP"]);
        assert_eq!(names(&registers(1 << 1, 0)), ["DACCVIOL"]);
        assert_eq!(names(&registers(0, 1 << 1)), ["VECTTBL"]);
    }

    #[test]
    fn a_valid_bus_address_is_reported() {
        let faulted = FaultRegisters {
            cfsr: (1 << 9) | BFARVALID,
            bfar: 0x6000_0000,
            ..FaultRegisters::default()
        };
        assert_eq!(fault_address(&faulted), Some(("BFAR", 0x6000_0000)));
        assert_eq!(
            summarize(5, &faulted),
            "BusFault: precise data bus error (PRECISERR) at 0x60000000 [BFAR]"
        );
    }

    #[test]
    fn stale_address_registers_are_ignored() {
        let faulted = FaultRegisters {
            cfsr: 1 << 16,
            bfar: 0x1234,
            mmfar: 0x5678,
            ..FaultRegisters::default()
        };
        assert_eq!(fault_address(&faulted), None);
    }

    #[test]
    fn an_escalated_fault_is_explained_by_its_real_cause() {
        let text = summarize(3, &registers(1 << 16, HFSR_FORCED));
        assert_eq!(text, "HardFault: undefined instruction (UNDEFINSTR)");
    }

    #[test]
    fn imprecise_errors_are_flagged() {
        assert!(is_imprecise(&registers(IMPRECISERR, 0)));
        assert!(!is_imprecise(&registers(1 << 9, 0)));
    }
}
