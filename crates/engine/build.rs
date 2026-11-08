//! Compiles FreeRTOS-shaped fixtures when an ARM cross compiler is available.
//! The directory names matter: they are how the port is detected.

use std::path::PathBuf;
use std::process::Command;

const COMPILER: &str = "arm-none-eabi-gcc";
const FIXTURES: [(&str, &str, &str); 2] = [
    ("ARM_CM3", "cortex-m3", "FIXTURE_CM3"),
    ("ARM_CM4F", "cortex-m4", "FIXTURE_CM4F"),
];

fn main() {
    println!("cargo::rustc-check-cfg=cfg(has_arm_gcc)");
    println!("cargo::rerun-if-changed=build.rs");
    println!("cargo::rerun-if-changed=tests/fixtures");

    let out_dir = PathBuf::from(std::env::var_os("OUT_DIR").expect("set by cargo"));
    for (port, cpu, variable) in FIXTURES {
        let output = out_dir.join(format!("{port}.elf"));
        let compiled = Command::new(COMPILER)
            .args([
                &format!("-mcpu={cpu}"),
                "-mthumb",
                "-O0",
                "-g",
                "-gdwarf-5",
                "-nostdlib",
                "-Wl,-e,entry",
                "-Wl,-Ttext=0x08000000",
                "-Wl,-Tdata=0x20000000",
                "-Wl,-Tbss=0x20001000",
            ])
            .arg(format!("tests/fixtures/{port}/port.c"))
            .arg("-o")
            .arg(&output)
            .status()
            .map(|status| status.success())
            .unwrap_or(false);
        if !compiled {
            println!("cargo::warning={COMPILER} failed; FreeRTOS fixture tests are skipped");
            return;
        }
        println!("cargo::rustc-env={variable}={}", output.display());
    }
    println!("cargo::rustc-cfg=has_arm_gcc");
}
