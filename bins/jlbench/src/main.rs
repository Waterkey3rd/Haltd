//! Measures what a J-Link and target can actually do (DESIGN §11).
//!
//! Read-only and sampling tests always run. Anything that stops the core or
//! writes flash needs an explicit flag, since the target may be driving
//! real hardware.

mod bench;

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use bench::{
    BenchOptions, FlashBpResult, FlashResult, HssResult, LatencyResult, RttResult,
    RunControlResult, Section, SpeedResult,
};
use clap::Parser;
use probe_api::{ConnectCfg, HistogramSummary, Interface, Probe, ProbeInfo, TargetInfo};
use probe_jlink::{JLink, JLinkOptions};
use serde::Serialize;

/// J-Link capability and latency benchmark.
#[derive(Parser)]
#[command(version, about)]
struct Cli {
    #[arg(long, default_value = "STM32H723VG")]
    device: String,
    /// Clock used for connecting and for every test except the speed ladder.
    #[arg(long, default_value_t = probe_api::DEFAULT_SPEED_KHZ)]
    speed: u32,
    /// Speeds to try, ascending. The V9 divides 12 MHz, so 8 and 15 MHz do not exist.
    #[arg(long, value_delimiter = ',', default_value = "4000,6000,12000")]
    ladder: Vec<u32>,
    #[arg(long)]
    serial: Option<u32>,
    #[arg(long)]
    dll: Option<PathBuf>,
    /// Start of a constant region, read back by the speed ladder.
    #[arg(long, value_parser = parse_u32, default_value = "0x08000000")]
    flash_addr: u32,
    /// RAM read by the latency tests. At least 16 KiB + 64 B must be mapped.
    #[arg(long, value_parser = parse_u32, default_value = "0x20000000")]
    ram_addr: u32,
    #[arg(long, default_value_t = 200)]
    iterations: usize,
    /// Permit tests that stop and step the core.
    #[arg(long)]
    allow_halt: bool,
    /// Permit tests that write flash. Needs --elf.
    #[arg(long)]
    allow_flash: bool,
    #[arg(long, default_value_t = 1000)]
    hss_period_us: u32,
    #[arg(long, default_value_t = 10.0)]
    hss_secs: f64,
    #[arg(long, default_value_t = 3.0)]
    rtt_secs: f64,
    /// RTT control block address; otherwise taken from --elf or scanned for.
    #[arg(long, value_parser = parse_u32)]
    rtt_addr: Option<u32>,
    #[arg(long)]
    elf: Option<PathBuf>,
    /// A slightly different build, to time programming only changed sectors.
    #[arg(long)]
    elf_alt: Option<PathBuf>,
    /// Function for the flash breakpoint test; pick one entered often, such as
    /// the SysTick handler's HAL_IncTick. Defaults to the one holding the PC.
    #[arg(long)]
    flash_bp_function: Option<String>,
    #[arg(long, default_value = "jlbench-report.json")]
    out: PathBuf,
}

fn parse_u32(text: &str) -> Result<u32, String> {
    let parsed = match text.strip_prefix("0x") {
        Some(hex) => u32::from_str_radix(hex, 16),
        None => text.parse(),
    };
    parsed.map_err(|e| e.to_string())
}

#[derive(Serialize)]
struct Report {
    probe: ProbeInfo,
    target: TargetInfo,
    speed_ladder: Section<Vec<SpeedResult>>,
    stable_speed_khz: Option<u32>,
    latency: Section<Vec<LatencyResult>>,
    run_control: Section<RunControlResult>,
    hss: Section<HssResult>,
    rtt: Section<RttResult>,
    flash: Section<FlashResult>,
    flash_breakpoint: Section<FlashBpResult>,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .init();

    let options = BenchOptions {
        speeds: cli.ladder.clone(),
        base_speed: cli.speed,
        flash_addr: cli.flash_addr,
        ram_addr: cli.ram_addr,
        iterations: cli.iterations,
        allow_halt: cli.allow_halt,
        allow_flash: cli.allow_flash,
        hss_period_us: cli.hss_period_us,
        hss_time: Duration::from_secs_f64(cli.hss_secs),
        rtt_time: Duration::from_secs_f64(cli.rtt_secs),
        rtt_control_block: cli.rtt_addr,
        elf: cli.elf.clone(),
        elf_alt: cli.elf_alt.clone(),
        flash_bp_function: cli.flash_bp_function.clone(),
    };

    let mut probe = JLink::open(&JLinkOptions {
        dll: cli.dll.clone(),
        serial: cli.serial,
        log_file: None,
    })
    .context("cannot open the J-Link")?;
    let target = probe
        .connect(&ConnectCfg {
            device: cli.device.clone(),
            interface: Interface::Swd,
            speed_khz: cli.speed,
        })
        .with_context(|| format!("cannot connect to {}", cli.device))?;

    let report = run(&mut probe, target, &options);
    println!("{}", render(&report));
    std::fs::write(&cli.out, serde_json::to_vec_pretty(&report)?)
        .with_context(|| format!("cannot write {}", cli.out.display()))?;
    println!("Full report: {}", cli.out.display());
    Ok(())
}

fn run(probe: &mut dyn Probe, target: TargetInfo, options: &BenchOptions) -> Report {
    eprintln!("speed ladder...");
    let ladder = bench::section_of(bench::speed_ladder(probe, options));
    let stable_speed_khz = match &ladder {
        Section::Done(results) => bench::stable_speed(results),
        _ => None,
    };
    eprintln!("latency...");
    let latency = bench::section_of(bench::latency(probe, options));
    let run_control = if options.allow_halt {
        eprintln!("run control...");
        bench::section_of(bench::run_control(probe, options))
    } else {
        Section::Skipped("needs --allow-halt".to_owned())
    };
    eprintln!("HSS ({:.0} s)...", options.hss_time.as_secs_f64());
    let hss = bench::section_of(bench::hss(probe, options));
    eprintln!("RTT...");
    let rtt = bench::rtt(probe, options);
    eprintln!("flash...");
    let flash = bench::flash(probe, options);
    eprintln!("flash breakpoints...");
    let flash_breakpoint = bench::flash_breakpoint(probe, options);
    Report {
        probe: probe.info(),
        target,
        speed_ladder: ladder,
        stable_speed_khz,
        latency,
        run_control,
        hss,
        rtt,
        flash,
        flash_breakpoint,
    }
}

fn row(h: &HistogramSummary) -> String {
    format!("{} | {} | {} | {}", h.count, h.mean_us, h.p99_us, h.max_us)
}

/// Status line for a section that did not produce a result.
fn not_done<T>(section: &Section<T>) -> Option<String> {
    match section {
        Section::Done(_) => None,
        Section::Skipped(reason) => Some(format!("Skipped: {reason}\n")),
        Section::Failed(reason) => Some(format!("**Failed:** {reason}\n")),
    }
}

fn render(report: &Report) -> String {
    use std::fmt::Write;
    let mut o = String::new();
    let probe = &report.probe;
    let _ = writeln!(o, "# J-Link capability report\n");
    let _ = writeln!(
        o,
        "- Probe: {} S/N {} HW {} ({})",
        probe.product, probe.serial, probe.hardware_version, probe.firmware
    );
    let _ = writeln!(o, "- DLL: V{}", probe.dll_version);
    let _ = writeln!(
        o,
        "- Target: {} CPUID {:#010x}, {} HW breakpoints, connected at {} kHz\n",
        report.target.device,
        report.target.cpuid,
        report.target.hw_breakpoints,
        report.target.speed_khz
    );

    let _ = writeln!(o, "## Speed ladder (4 KiB flash reads)\n");
    match &report.speed_ladder {
        Section::Done(results) => {
            let _ = writeln!(
                o,
                "requested kHz | actual kHz | errors | KB/s\n---|---|---|---"
            );
            for r in results {
                let _ = writeln!(
                    o,
                    "{} | {} | {}/{} | {:.0}",
                    r.requested_khz, r.actual_khz, r.errors, r.rounds, r.kilobytes_per_second
                );
                if let Some(error) = &r.first_error {
                    let _ = writeln!(o, "\nfirst error: {error}\n");
                }
            }
            let _ = writeln!(o, "\nStable: {:?} kHz\n", report.stable_speed_khz);
        }
        other => o.push_str(&not_done(other).unwrap_or_default()),
    }

    let _ = writeln!(o, "## Latency (µs)\n");
    match &report.latency {
        Section::Done(results) => {
            let _ = writeln!(o, "operation | n | mean | p99 | max\n---|---|---|---|---");
            for r in results {
                let _ = writeln!(o, "{} | {}", r.operation, row(&r.latency));
            }
            o.push('\n');
        }
        other => o.push_str(&not_done(other).unwrap_or_default()),
    }

    let _ = writeln!(o, "## Run control (µs)\n");
    match &report.run_control {
        Section::Done(r) => {
            let _ = writeln!(o, "operation | n | mean | p99 | max\n---|---|---|---|---");
            let _ = writeln!(o, "halt, confirmed | {}", row(&r.halt));
            let _ = writeln!(o, "go, confirmed | {}", row(&r.go));
            let _ = writeln!(o, "step | {}\n", row(&r.step));
        }
        other => o.push_str(&not_done(other).unwrap_or_default()),
    }

    let _ = writeln!(o, "## HSS\n");
    match &report.hss {
        Section::Done(r) => {
            let _ = writeln!(
                o,
                "- Caps: max_blocks {}, max_freq {}, caps {:#x}, raw {:x?}",
                r.caps.max_blocks, r.caps.max_freq_hz, r.caps.caps, r.caps.raw
            );
            for f in &r.formats {
                let _ = writeln!(
                    o,
                    "- flags {}: {} bytes, format {:?}, second block ok {:?}, header delta {:?}",
                    f.flags, f.captured_bytes, f.format, f.second_block_matches, f.header_delta
                );
                let _ = writeln!(o, "  first bytes: {}", f.first_bytes_hex);
            }
            let _ = writeln!(
                o,
                "- Sustained: {} blocks @ {} µs for {:.1} s: {} records, lost {:?}, {:.0} B/s",
                r.sustained_blocks,
                r.period_us,
                r.seconds,
                r.records,
                r.lost_records,
                r.bytes_per_second
            );
            let _ = writeln!(
                o,
                "- ReadMemEx during HSS: {} ok, {} failed; {}\n",
                r.memory_reads_ok,
                r.memory_reads_failed,
                row(&r.memory_read_latency)
            );
        }
        other => o.push_str(&not_done(other).unwrap_or_default()),
    }

    let _ = writeln!(o, "## RTT\n");
    match &report.rtt {
        Section::Done(r) => {
            let _ = writeln!(
                o,
                "{} up channels, {:.0} B/s over {:.1} s, {} host overflows\n",
                r.up_channels, r.bytes_per_second, r.seconds, r.host_overflows
            );
        }
        other => o.push_str(&not_done(other).unwrap_or_default()),
    }

    let _ = writeln!(o, "## Flash\n");
    match &report.flash {
        Section::Done(r) => {
            let _ = writeln!(
                o,
                "unchanged image: {:.0} ms; changed: {:?} ms; restore: {:?} ms\n",
                r.unchanged_ms, r.changed_ms, r.restore_ms
            );
        }
        other => o.push_str(&not_done(other).unwrap_or_default()),
    }

    let _ = writeln!(o, "## Flash breakpoints (µs)\n");
    match &report.flash_breakpoint {
        Section::Done(r) => {
            let _ = writeln!(
                o,
                "At {} ({:#010x}): hit {}/{} rounds\n\noperation | n | mean | p99 | max\n---|---|---|---|---",
                r.function, r.address, r.hits, r.rounds
            );
            let _ = writeln!(o, "set | {}", row(&r.set_bp));
            let _ = writeln!(o, "go with BP | {}", row(&r.go_with_bp));
            let _ = writeln!(o, "step off BP | {}", row(&r.step_off_bp));
            let _ = writeln!(o, "clear | {}", row(&r.clear_bp));
            let _ = writeln!(o, "go after clear | {}\n", row(&r.go_after_clear));
        }
        other => o.push_str(&not_done(other).unwrap_or_default()),
    }
    o
}
