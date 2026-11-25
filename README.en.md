<p align="center">
  <img src="assets/logo.svg" alt="haltd" width="460" />
</p>

<p align="center">
  <a href="README.md">中文</a> | <strong>English</strong>
</p>

# Haltd

A J-Link debug backend, VS Code extension, and CLI tool for STM32 (ARM Cortex-M), built around a resident daemon + DAP + WebSocket data-channel architecture.

## Overview

`Haltd` is developed as an **open-source alternative to Ozone**. Using a resident daemon + DAP + WebSocket architecture, it brings fast symbol caching, `-O2` stepping, HardFault decoding, FreeRTOS awareness, 1 kHz live waveform plotting, and structured RTT decoding directly into VS Code and CLI workflows:

- **Smooth `-O2` debugging**: handles optimized code stepping, inline virtual stack frames, and `<optimized out>` reporting cleanly.
- **Non-intrusive live observation**: view and modify Live Watch variables, plot 1 kHz HSS / polled waveforms in Timeline, and stream decoded RTT logs without halting the MCU—alongside regular breakpoints, HardFault diagnosis, and FreeRTOS task inspection.
- **Fast ELF loading & Build & Reload**: parallel DWARF indexing with on-disk `blake3` caching keeps large-ELF startup sub-second, and `F6` flashes only changed sectors before rebinding breakpoints and watches.
- **Consistent Linux & Windows workflow**: lightweight, open-source, and scriptable across both platforms.

Core daily debugging workflows are ready to serve as a primary debug environment, and `Haltd` also complements Ozone nicely (including reusing SVD files from an existing Ozone installation). Shared in the spirit of open-source exchange (not commercial software). Due to limited hardware on hand, not every MCU or probe variant could be tested—issue reports, real-hardware test results, and PRs adding device profiles (`devices/*.yaml`) are warmly welcome.

---

## Platform & Hardware Status

| Target / Component | Status | Notes |
|---|---|---|
| **Operating Systems** | Linux x64 / Windows x64 | Both supported and tested; Linux keeps `mmap` and zero-overhead paths, while Windows uses isolated `#[cfg(windows)]` logic |
| **J-Link Probes** | Verified on J-Link V9 | Other J-Link models should work; feedback welcome |
| **STM32F1 (F103C8)** | Verified | Full debugging, HSS, RTT, Fault, and FreeRTOS tested |
| **STM32G4 Series** | Verified | Built-in profile: [devices/stm32g4.yaml](devices/stm32g4.yaml) |
| **STM32H72x / H73x (H723)** | Verified | Built-in profile: [devices/stm32h7.yaml](devices/stm32h7.yaml) |
| **STM32H74x / H75x** | Built-in, needs full hardware verification | Profile written from RM0433 ([devices/stm32h74.yaml](devices/stm32h74.yaml)), not yet fully tested on hardware |
| **Other Cortex-M / STM32** | Basic connection supported | Can connect and debug; built-in `DBGMCU` / `freezeOnHalt` profiles are not included yet |

> To benchmark your own probe and MCU, see the built-in `jlbench` tool in [J-Link Notes](docs/jlink-notes.md), or contribute a `devices/*.yaml` profile via PR.

---

## Core Features

- **Source-Level Debugging (DAP)**: `-O2` line stepping and inline frame unwinding; conditional breakpoints, hit counts, logpoints, direct-DWT hardware watchpoints, and one-key `F6` Build & Reload.
- **HardFault Interception & Decoding**: halts on fault entry, decodes `CFSR` / `HFSR` / `MMFAR` / `BFAR` / `AFSR`, and unwinds past `<exception N>` straight to the offending source line.
- **FreeRTOS Awareness**: reads TCB and task list layouts directly from DWARF (CM3 / CM4F) with no hardcoded offsets; displays task states, priorities, stack high-water marks, and per-task call stacks.
- **Live Watch & Timeline**:
  - **Live Watch**: docked in the bottom Panel by default (with a titlebar **Pop out** button to open an editor tab beside your source code), featuring an Ozone-style multi-column tree-table (`Expression`, `Value` with inline `auto`/`dec`/`hex` format tag, configurable `Type`, `Address`, `Rate` across `0–20 Hz` defaulting to `5 Hz`), text-only freshness coloring (running cool-down + halted-step change highlight), ELF global symbol autocomplete, dynamic pointer/array & arithmetic expression watching, DWT hardware watchpoints & memory viewer links, and Ozone-style shortcuts (`Ctrl+G` add to Timeline, `Ctrl+H` add to Live Watch, etc.).
  - **Timeline (HSS / Polling / RTT)**: up to 1 kHz HSS sampling (overflow falls back to polling) rendered in an Ozone Timeline / Data Graph-inspired multi-track oscilloscope UI unifying HSS and RTT signals, with hover value boxes, sample/delta cursors, drag-and-drop track grouping, reusable workspace **Presets** (`.haltd/timeline-presets.json`, replace or `+` append), dark/light themes, `.hlrec` live recording timer & post-stop banner, and smart CSV/PNG export (Cursor `A..B` default, visible-only filter, ZOH alignment, and direct `.hlrec -> CSV`).
- **RTT Terminal & Structured Decoding**: besides ANSI text, regex, `.haltd/rtt.yaml`, Lua, and SystemView, supports zero-config auto-decoding from ELF struct types and `key=value` text with automatic Timeline plotting (configurable in `launch.json`).
- **VS Code Device Selection Assist**: provides J-Link device completion, core/Flash/RAM + built-in profile hover info, unknown-device warnings in `launch.json`, and `Haltd: Select Device`.
- **CMSIS-SVD Peripherals View**: protects registers with read side-effects (such as `DR` / `RDR` / `RXDR`) from automatic reads, with field-level read-modify-write editing.
- **C++ Type Visualizers**: built-in Lua rules for `units`, `Eigen::Matrix` / `Quaternion`, `std::array` / `std::tuple` / `std::optional` / `std::variant` / `std::span` / `std::string_view`, plus custom project rules.

---

## Installation & Quick Start

### Option 1: Prebuilt Releases (Recommended)

Download the archive for your OS from Releases:
- **Linux x64**: `haltd-<version>-linux-x64.vsix` + `haltd` binary
- **Windows x64**: `haltd-<version>-win32-x64.vsix` + `haltd.exe` binary

You only need the **SEGGER J-Link Software** driver and **VS Code (≥ 1.90)** installed. Each `.vsix` bundles the backend binary—no Rust, Node.js, or VC++ runtime required on the target machine.

See the **[User Guide (zh-CN)](docs/usage.md)** for `launch.json`, shortcuts, `.haltd/project.yaml`, `.haltd/rtt.yaml`, and CLI usage.

### Option 2: Build from Source

Requirements: Rust toolchain (`rust-version = 1.85`), Node.js / npm (for `webui/` and `vscode-ext/`), and optionally `arm-none-eabi-gcc` for running DWARF / FreeRTOS fixture tests (automatically skipped if absent).

**Linux x64**:

```bash
# 1. Build the embedded Web UI first (server embeds webui/dist at compile time)
cd webui && npm ci && npm run build && cd ..

# 2. Build and install haltd into ~/.cargo/bin
cargo install --path bins/haltd --locked

# 3. Build the VS Code extension (optional)
cd vscode-ext && npm ci && npm run build && cd ..
```

**Windows x64 (PowerShell)**:

```powershell
# Requires Rust (MSVC) and Node.js; outputs packages into release/
powershell -ExecutionPolicy Bypass -File scripts\release-windows.ps1
```

---

## Documentation

- **[Architecture & Design (zh-CN)](docs/DESIGN.md)**: overall architecture, core modules (symbol indexing, `-O2` stepping, HSS planning & clock sync, RTT decoding, TSDB), DAP / WebSocket protocols, and configuration specs.
- **[User Guide (zh-CN)](docs/usage.md)**: installation, VS Code debug configuration, Live Watch / Timeline, RTT decoding, FreeRTOS / SVD, CLI commands, and troubleshooting (Linux & Windows).
- **[J-Link Notes & Measured Behaviors (zh-CN)](docs/jlink-notes.md)**: measured J-Link quirks (SWD clock dividers, HSS budget throttling, `JLINKARM_SetWP` clearing `DEMCR`, RTT reset state, Windows `CP_ACP` paths) and `jlbench` benchmarking instructions.
- **[License](LICENSE)**: MIT License for Haltd; at runtime it dynamically loads the user-installed SEGGER J-Link library (`libjlinkarm.so` / `JLink_x64.dll`), whose use remains subject to SEGGER's own license terms.

---

## Development & Tests

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

Most unit tests run against the built-in `probe-mock` backend without requiring physical hardware.
