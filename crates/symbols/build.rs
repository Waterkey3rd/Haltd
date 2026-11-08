//! Compiles the DWARF test fixtures when an ARM cross compiler is available.

use std::path::{Path, PathBuf};
use std::process::Command;

const COMPILER: &str = "arm-none-eabi-g++";
const SOURCES: [&str; 2] = ["tests/fixtures/basic.cpp", "tests/fixtures/opaque.cpp"];

fn main() {
    println!("cargo::rustc-check-cfg=cfg(has_arm_gcc)");
    for source in SOURCES {
        println!("cargo::rerun-if-changed={source}");
    }
    println!("cargo::rerun-if-changed=build.rs");

    let output =
        PathBuf::from(std::env::var_os("OUT_DIR").expect("set by cargo")).join("basic.elf");
    if compile(&output) {
        println!("cargo::rustc-cfg=has_arm_gcc");
        println!("cargo::rustc-env=FIXTURE_ELF={}", output.display());
    } else {
        println!("cargo::warning={COMPILER} is unavailable; DWARF fixture tests are skipped");
    }
}

fn compile(output: &Path) -> bool {
    // Mirrors the firmware's RelWithDebInfo flags (DESIGN §2).
    Command::new(COMPILER)
        .args([
            "-mcpu=cortex-m7",
            "-mthumb",
            "-mfpu=fpv5-d16",
            "-mfloat-abi=hard",
            "-std=c++20",
            "-O2",
            "-g",
            "-gdwarf-5",
            "-ffunction-sections",
            "-fdata-sections",
            "-fno-rtti",
            "-fno-exceptions",
            "-nostdlib",
            "-Wl,--gc-sections",
            "-Wl,-e,entry",
            "-Wl,-Ttext=0x08000000",
            "-Wl,-Tdata=0x20000000",
            // Garbage-collection roots: everything fixture.rs reads.
            "-Wl,-u,_ZN2fw7g_stateE",
            "-Wl,-u,_ZN2fw8g_bufferE",
            "-Wl,-u,_ZN2fw6Widget7s_countE",
            "-Wl,-u,g_counter",
            "-Wl,-u,g_state_ptr",
            "-Wl,-u,g_state_ref",
            "-Wl,-u,g_opaque",
            "-Wl,-u,prvTaskExitError",
        ])
        .args(SOURCES)
        .arg("-o")
        .arg(output)
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}
