use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use clap::{Args, Parser, Subcommand, ValueEnum};
use engine::rtos::freertos::{FreeRtos, TaskState};
use probe_api::{
    AccessWidth, BpKind, ConnectCfg, Interface, Probe, ProbeInfo, Progress, Reg, ResetKind,
    TargetInfo, DEFAULT_SPEED_KHZ,
};
use probe_jlink::{JLink, JLinkOptions};
use symbols::{
    CoreRegisters, FormatOptions, FrameKind, LoadOptions, MemoryReader, StackFrame, SymbolDb,
};

const BREAKPOINT_TIMEOUT: Duration = Duration::from_secs(3);

/// J-Link debug backend for STM32.
#[derive(Parser)]
#[command(version, about)]
struct Cli {
    /// Log filter, e.g. "info" or "jlink=trace".
    #[arg(long, global = true, default_value = "warn", env = "JLINKD_LOG")]
    log: String,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Show the probe and, unless --no-target is given, the connected target.
    Info {
        #[command(flatten)]
        connection: Connection,
        /// Query only the probe; do not attempt to attach to a target.
        #[arg(long)]
        no_target: bool,
    },
    /// Program an ELF or BIN file, rewriting only changed flash sectors.
    Flash {
        file: PathBuf,
        #[command(flatten)]
        connection: Connection,
        /// Leave the core halted after programming.
        #[arg(long)]
        no_run: bool,
    },
    /// Read memory by address or a variable by expression, without halting.
    Read {
        /// An address such as 0x20000000, or an expression such as
        /// "g_state.items[2].gain".
        target: String,
        /// ELF file providing symbols; required for expressions.
        #[arg(long)]
        elf: Option<PathBuf>,
        /// Number of items to read when given an address.
        #[arg(long, default_value_t = 1)]
        count: usize,
        /// Item size in bytes when given an address.
        #[arg(long, default_value = "4", value_parser = ["1", "2", "4"])]
        width: String,
        #[command(flatten)]
        connection: Connection,
    },
    /// List FreeRTOS tasks. Without --halt the target keeps running and the
    /// snapshot may be inconsistent.
    Tasks {
        #[arg(long)]
        elf: PathBuf,
        /// Stop the core during the snapshot, restoring it afterwards.
        #[arg(long)]
        halt: bool,
        /// Print each task's call stack; implies --halt.
        #[arg(long)]
        backtrace: bool,
        /// Run to a hardware breakpoint on this function first; implies --halt.
        #[arg(long, value_name = "FUNCTION")]
        break_at: Option<String>,
        #[command(flatten)]
        connection: Connection,
    },
    /// Run the background process that owns the probe.
    Serve {
        /// HTTP and WebSocket port.
        #[arg(long, default_value_t = server::DEFAULT_PORT)]
        port: u16,
        #[arg(long, default_value_t = server::DEFAULT_PORT + server::DAP_PORT_OFFSET)]
        dap_port: u16,
        /// Exit after this long unused; 0 keeps running, as for a system service.
        #[arg(long, default_value_t = 30)]
        idle_exit_secs: u64,
    },
    /// Debug Adapter Protocol on stdio, forwarded to the background process.
    Dap {
        #[arg(long, default_value_t = server::DEFAULT_PORT + server::DAP_PORT_OFFSET)]
        dap_port: u16,
    },
    /// Load an ELF and report index statistics; no probe is needed.
    Symbols {
        elf: PathBuf,
        /// Rebuild the index, ignoring the cache.
        #[arg(long)]
        no_cache: bool,
        /// Names of globals or functions to look up.
        #[arg(long)]
        lookup: Vec<String>,
    },
}

#[derive(Clone, Copy, ValueEnum)]
enum InterfaceArg {
    Swd,
    Jtag,
}

#[derive(Args)]
struct Connection {
    #[arg(long, default_value = "STM32H723VG", env = "JLINKD_DEVICE")]
    device: String,
    #[arg(long, value_enum, default_value_t = InterfaceArg::Swd)]
    interface: InterfaceArg,
    /// Interface clock in kHz.
    #[arg(long, default_value_t = DEFAULT_SPEED_KHZ)]
    speed: u32,
    /// Serial number, when several probes are attached.
    #[arg(long)]
    serial: Option<u32>,
    /// Path of the J-Link library (libjlinkarm.so or JLink_x64.dll).
    #[arg(long, env = "JLINKD_DLL")]
    dll: Option<PathBuf>,
}

impl Connection {
    fn open_probe(&self) -> Result<JLink> {
        JLink::open(&JLinkOptions {
            dll: self.dll.clone(),
            serial: self.serial,
            log_file: None,
        })
        .context("cannot open the J-Link")
    }

    fn config(&self) -> ConnectCfg {
        ConnectCfg {
            device: self.device.clone(),
            interface: match self.interface {
                InterfaceArg::Swd => Interface::Swd,
                InterfaceArg::Jtag => Interface::Jtag,
            },
            speed_khz: self.speed,
        }
    }

    fn connect(&self) -> Result<JLink> {
        let mut probe = self.open_probe()?;
        let config = self.config();
        probe.connect(&config).with_context(|| {
            format!(
                "cannot connect to {} via {} at {} kHz",
                config.device,
                match config.interface {
                    Interface::Swd => "SWD",
                    Interface::Jtag => "JTAG",
                },
                config.speed_khz
            )
        })?;
        Ok(probe)
    }
}

struct ProbeMemory<'a>(&'a mut dyn Probe);

impl MemoryReader for ProbeMemory<'_> {
    fn read(&mut self, addr: u32, buf: &mut [u8]) -> Result<(), String> {
        self.0
            .read_mem(addr, buf, AccessWidth::Auto)
            .map_err(|e| e.to_string())
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::new(&cli.log))
        .with_writer(std::io::stderr)
        .init();

    match cli.command {
        Command::Info {
            connection,
            no_target,
        } => info(&connection, no_target),
        Command::Flash {
            file,
            connection,
            no_run,
        } => flash(&file, &connection, no_run),
        Command::Read {
            target,
            elf,
            count,
            width,
            connection,
        } => {
            let width: usize = width.parse().context("invalid width")?;
            read(&target, elf.as_deref(), count, width, &connection)
        }
        Command::Tasks {
            elf,
            halt,
            backtrace,
            break_at,
            connection,
        } => tasks(
            &elf,
            halt || backtrace || break_at.is_some(),
            backtrace,
            break_at.as_deref(),
            &connection,
        ),
        Command::Serve {
            port,
            dap_port,
            idle_exit_secs,
        } => {
            let idle_limit = (idle_exit_secs > 0).then(|| Duration::from_secs(idle_exit_secs));
            Ok(server::daemon::serve(port, dap_port, idle_limit)?)
        }
        Command::Dap { dap_port } => Ok(server::bridge::run(dap_port)?),
        Command::Symbols {
            elf,
            no_cache,
            lookup,
        } => symbols_report(&elf, no_cache, &lookup),
    }
}

fn print_probe(info: &ProbeInfo) {
    println!("Probe");
    println!("  product      {}", info.product);
    println!("  serial       {}", info.serial);
    println!("  firmware     {}", info.firmware);
    println!("  hardware     {}", info.hardware_version);
    println!("  DLL          V{}", info.dll_version);
    println!("  caps         {:#010x}", info.caps);
    println!("  capabilities {}", info.cap_names.join(" "));
    if !info.missing_symbols.is_empty() {
        println!("  missing      {}", info.missing_symbols.join(" "));
    }
}

fn print_target(target: &TargetInfo) {
    println!("Target");
    println!("  device       {}", target.device);
    println!("  speed        {} kHz", target.speed_khz);
    println!("  CPUID        {:#010x}", target.cpuid);
    println!("  HW breakpoints {}", target.hw_breakpoints);
    println!(
        "  state        {}",
        if target.halted { "halted" } else { "running" }
    );
}

fn info(connection: &Connection, no_target: bool) -> Result<()> {
    let mut probe = connection.open_probe()?;
    print_probe(&probe.info());
    if no_target {
        return Ok(());
    }
    // Reaching the probe but not the target is a useful result by itself, so
    // this is reported rather than failing the whole command.
    match probe.connect(&connection.config()) {
        Ok(target) => print_target(&target),
        Err(error) => println!("Target\n  not connected: {error}"),
    }
    Ok(())
}

fn flash(file: &Path, connection: &Connection, no_run: bool) -> Result<()> {
    if !file.is_file() {
        bail!("{} is not a file", file.display());
    }
    let mut probe = connection.connect()?;
    let stats = probe
        .download(file, &|progress: Progress| {
            eprint!(
                "\r{:<12} {:>3}% {:<32}",
                progress.action, progress.percent, progress.item
            );
        })
        .with_context(|| format!("cannot program {}", file.display()))?;
    eprintln!();
    probe.reset(ResetKind::Normal, no_run)?;
    println!(
        "download finished in {:.2?}; target {}",
        stats.elapsed,
        if no_run { "halted" } else { "running" }
    );
    Ok(())
}

fn parse_address(text: &str) -> Option<u32> {
    let digits = text
        .strip_prefix("0x")
        .or_else(|| text.strip_prefix("0X"))?;
    u32::from_str_radix(digits, 16).ok()
}

fn read(
    target: &str,
    elf: Option<&Path>,
    count: usize,
    width: usize,
    connection: &Connection,
) -> Result<()> {
    if let Some(address) = parse_address(target) {
        let mut probe = connection.connect()?;
        return read_address(&mut probe, address, count, width);
    }

    let elf = elf.context("reading an expression requires --elf")?;
    // Load symbols first: a typo should not need the target.
    let db = SymbolDb::load(elf, &LoadOptions::default())
        .with_context(|| format!("cannot load {}", elf.display()))?;
    let mut probe = connection.connect()?;
    let result = db.read_expression(
        target,
        &mut ProbeMemory(&mut probe),
        &FormatOptions::default(),
    )?;
    println!("{target} = {}", result.value);
    match result.addr {
        Some(address) => println!("  ({} @ {address:#010x})", result.type_name),
        None => println!("  ({})", result.type_name),
    }
    Ok(())
}

fn current_registers(probe: &mut dyn Probe) -> Result<CoreRegisters> {
    let mut ids: Vec<Reg> = (0..16).map(Reg::R).collect();
    ids.extend([Reg::Xpsr, Reg::Msp, Reg::Psp]);
    let mut values = [0u32; 19];
    probe.read_regs(&ids, &mut values)?;
    let mut registers = CoreRegisters {
        xpsr: values[16],
        msp: values[17],
        psp: values[18],
        ..CoreRegisters::default()
    };
    registers.r.copy_from_slice(&values[..16]);
    Ok(registers)
}

fn print_backtrace(frames: &[StackFrame]) {
    for (depth, frame) in frames.iter().enumerate() {
        match frame.kind {
            FrameKind::Exception { number } => {
                println!("    #{depth:<2} <exception {number}> sp={:#010x}", frame.sp);
            }
            FrameKind::Code => println!(
                "    #{depth:<2} {:#010x} {} sp={:#010x}",
                frame.pc,
                frame.function.as_deref().unwrap_or("??"),
                frame.sp
            ),
        }
    }
}

fn print_tasks(
    db: &SymbolDb,
    rtos: &FreeRtos,
    probe: &mut dyn Probe,
    backtrace: bool,
) -> Result<()> {
    let list = rtos.tasks(&mut ProbeMemory(&mut *probe))?;
    println!(
        "{:<16} {:<10} {:>5} {:>10} {:>10} {:>11}",
        "NAME", "STATE", "PRIO", "TCB", "STACK", "FREE BYTES"
    );
    for task in &list.tasks {
        println!(
            "{:<16} {:<10} {:>5} {:#010x} {:#010x} {:>11}",
            task.name,
            task.state.name(),
            task.priority,
            task.tcb,
            task.stack_base,
            task.stack_free_bytes
        );
        if backtrace {
            let registers = if task.state == TaskState::Running {
                current_registers(probe)?
            } else {
                rtos.saved_context(task, &mut ProbeMemory(&mut *probe))?
            };
            print_backtrace(&db.unwind(&registers, &mut ProbeMemory(&mut *probe))?);
        }
    }
    if list.tasks.len() != list.kernel_task_count as usize {
        println!(
            "warning: found {} tasks but the kernel counts {}",
            list.tasks.len(),
            list.kernel_task_count
        );
    }
    Ok(())
}

/// Resume, wait for a hardware breakpoint at `function`, and remove it.
fn run_to(probe: &mut dyn Probe, db: &SymbolDb, function: &str) -> Result<()> {
    let address = db
        .functions(function)
        .first()
        .map(|found| found.low)
        .with_context(|| format!("no function named {function}"))?;
    let breakpoint = probe.set_bp(address, BpKind::Hw)?;
    let reached = (|| -> Result<bool> {
        probe.go()?;
        let started = Instant::now();
        while started.elapsed() < BREAKPOINT_TIMEOUT {
            if probe.is_halted()? {
                return Ok(true);
            }
        }
        probe.halt()?;
        Ok(false)
    })();
    probe.clear_bp(breakpoint)?;
    if !reached? {
        bail!("{function} was not reached within {BREAKPOINT_TIMEOUT:?}");
    }
    Ok(())
}

fn tasks(
    elf: &Path,
    halt: bool,
    backtrace: bool,
    break_at: Option<&str>,
    connection: &Connection,
) -> Result<()> {
    let db = SymbolDb::load(elf, &LoadOptions::default())
        .with_context(|| format!("cannot load {}", elf.display()))?;
    let rtos = FreeRtos::detect(&db)?.context("this image does not contain FreeRTOS")?;
    let mut probe = connection.connect()?;
    let resume = halt && !probe.is_halted()?;
    if halt {
        probe.halt()?;
    }
    let printed = match break_at {
        Some(function) => run_to(&mut probe, &db, function),
        None => Ok(()),
    }
    .and_then(|()| print_tasks(&db, &rtos, &mut probe, backtrace));
    if resume {
        probe.go()?;
    }
    printed
}

fn read_address(probe: &mut dyn Probe, address: u32, count: usize, width: usize) -> Result<()> {
    let access = match width {
        1 => AccessWidth::U8,
        2 => AccessWidth::U16,
        _ => AccessWidth::U32,
    };
    let mut buffer = vec![0u8; count * width];
    probe.read_mem(address, &mut buffer, access)?;
    for (line, row) in buffer.chunks(16).enumerate() {
        print!("{:#010x}:", address + (line * 16) as u32);
        for item in row.chunks(width) {
            let value = item
                .iter()
                .rev()
                .fold(0u32, |acc, byte| (acc << 8) | u32::from(*byte));
            print!(" {value:0digits$x}", digits = width * 2);
        }
        println!();
    }
    Ok(())
}

fn symbols_report(elf: &Path, no_cache: bool, lookups: &[String]) -> Result<()> {
    let db = SymbolDb::load(
        elf,
        &LoadOptions {
            no_cache,
            ..LoadOptions::default()
        },
    )
    .with_context(|| format!("cannot load {}", elf.display()))?;
    let stats = db.stats();
    println!("{}", elf.display());
    println!("  hash         {}", db.hash());
    println!(
        "  load         {:.1?} total ({:.1?} hashing, {:.1?} {})",
        stats.total,
        stats.hash,
        stats.index,
        if stats.cache_hit {
            "reading cache"
        } else {
            "building index"
        }
    );
    println!("  units        {}", stats.units);
    println!("  globals      {}", stats.globals);
    println!("  functions    {}", stats.functions);
    println!("  types        {}", stats.types);
    println!("  files        {}", stats.files);
    println!("  strings      {} KiB", stats.string_bytes / 1024);

    if let Some(rtos) = FreeRtos::detect(&db)? {
        println!(
            "  FreeRTOS     {:?} port, {} priority levels",
            rtos.port(),
            rtos.priority_levels()
        );
    }

    for name in lookups {
        let globals = db.globals(name);
        let functions = db.functions(name);
        let symbol = db.elf_symbol(name);
        if globals.is_empty() && functions.is_empty() && symbol.is_none() {
            println!("{name}: not found");
        }
        for variable in &globals {
            println!(
                "{name}: variable @ {:#010x}, {} bytes",
                variable.addr, variable.size
            );
        }
        for function in &functions {
            println!(
                "{name}: function {:#010x}..{:#010x}",
                function.low, function.high
            );
        }
        if let (true, Some((address, size))) = (globals.is_empty() && functions.is_empty(), symbol)
        {
            println!("{name}: ELF symbol @ {address:#010x}, {size} bytes");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_hex_is_treated_as_an_address() {
        assert_eq!(parse_address("0x20000000"), Some(0x2000_0000));
        assert_eq!(parse_address("0X10"), Some(0x10));
        assert_eq!(parse_address("g_state"), None);
        assert_eq!(parse_address("123"), None);
        assert_eq!(parse_address("0xZZ"), None);
    }

    #[test]
    fn defaults_are_swd_at_4_mhz() {
        let cli = Cli::parse_from(["jlinkd", "info"]);
        let Command::Info { connection, .. } = cli.command else {
            panic!("expected info");
        };
        let config = connection.config();
        assert_eq!(config.interface, Interface::Swd);
        assert_eq!(config.speed_khz, 4000);
        assert_eq!(config.device, "STM32H723VG");
    }
}
