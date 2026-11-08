//! Compiles a fixture of template types when an ARM compiler is available.

use std::path::PathBuf;
use std::process::Command;

fn main() {
    println!("cargo::rustc-check-cfg=cfg(has_arm_gcc)");
    println!("cargo::rerun-if-changed=build.rs");
    println!("cargo::rerun-if-changed=tests/fixtures/templates.cpp");
    let output = PathBuf::from(std::env::var_os("OUT_DIR").expect("set by cargo")).join("t.elf");
    let compiled = Command::new("arm-none-eabi-g++")
        .args([
            "-mcpu=cortex-m7",
            "-mthumb",
            "-mfpu=fpv5-d16",
            "-mfloat-abi=hard",
            "-std=c++20",
            "-O0",
            "-g",
            "-gdwarf-5",
            "-fno-rtti",
            "-fno-exceptions",
            "-nostdlib",
            "-Wl,-e,entry",
            "-Wl,-Ttext=0x08000000",
            "-Wl,-Tdata=0x20000000",
            "tests/fixtures/templates.cpp",
            "-o",
        ])
        .arg(&output)
        .status()
        .map(|status| status.success())
        .unwrap_or(false);
    if compiled {
        println!("cargo::rustc-cfg=has_arm_gcc");
        println!("cargo::rustc-env=FIXTURE_ELF={}", output.display());
    } else {
        println!("cargo::warning=template fixture was not built; its tests are skipped");
    }
}
