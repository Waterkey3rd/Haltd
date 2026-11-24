//! `haltd dap`: connects an editor's stdio to the daemon, starting it if
//! needed. Messages pass through unchanged.

use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::net::{Ipv4Addr, Shutdown, TcpStream};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

const START_TIMEOUT: Duration = Duration::from_secs(5);
const RETRY_INTERVAL: Duration = Duration::from_millis(50);

fn connect(port: u16) -> io::Result<TcpStream> {
    TcpStream::connect((Ipv4Addr::LOCALHOST, port))
}

fn log_file() -> Option<File> {
    let directory = symbols::paths::cache_root()?;
    fs::create_dir_all(&directory).ok()?;
    File::options()
        .create(true)
        .append(true)
        .open(directory.join("serve.log"))
        .ok()
}

fn start_daemon(port: u16) -> io::Result<()> {
    let program = std::env::current_exe()?;
    let (stdout, stderr) = match log_file() {
        Some(file) => (Stdio::from(file.try_clone()?), Stdio::from(file)),
        None => (Stdio::null(), Stdio::null()),
    };
    let mut command = Command::new(program);
    command
        .args(["--log", "info", "serve", "--dap-port", &port.to_string()])
        .stdin(Stdio::null())
        .stdout(stdout)
        .stderr(stderr);
    spawn_detached(&mut command)
}

#[cfg(not(windows))]
fn spawn_detached(command: &mut Command) -> io::Result<()> {
    command.spawn().map(|_| ())
}

/// No console window, and outliving the editor that started us. Leaving the
/// editor's job object is refused when the job forbids it, so that is optional.
#[cfg(windows)]
fn spawn_detached(command: &mut Command) -> io::Result<()> {
    use std::os::windows::process::CommandExt;

    const DETACHED_PROCESS: u32 = 0x0000_0008;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;

    let flags = DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP;
    command.creation_flags(flags | CREATE_BREAKAWAY_FROM_JOB);
    if command.spawn().is_ok() {
        return Ok(());
    }
    command.creation_flags(flags).spawn().map(|_| ())
}

/// Stop a backend left running by another build of this program. It can
/// outlive an upgrade for as long as a Timeline page stays connected, and
/// would then serve new sessions with the old code.
#[cfg(target_os = "linux")]
fn stop_stale_daemon(port: u16) {
    let Ok(own) = std::env::current_exe() else {
        return;
    };
    let wanted = format!("serve\0--dap-port\0{port}\0");
    let Ok(processes) = fs::read_dir("/proc") else {
        return;
    };
    for entry in processes.flatten() {
        let directory = entry.path();
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<u32>().ok())
        else {
            continue;
        };
        let is_backend = fs::read(directory.join("cmdline"))
            .is_ok_and(|line| String::from_utf8_lossy(&line).contains(&wanted));
        // A replaced executable reads as its old path with " (deleted)" added.
        let same_build = fs::read_link(directory.join("exe")).is_ok_and(|exe| exe == own);
        if pid == std::process::id() || !is_backend || same_build {
            continue;
        }
        tracing::info!("stopping backend {pid}, which is a different build");
        let _ = Command::new("kill").arg(pid.to_string()).status();
        let stopped = Instant::now();
        while directory.exists() && stopped.elapsed() < START_TIMEOUT {
            thread::sleep(RETRY_INTERVAL);
        }
    }
}

#[cfg(not(target_os = "linux"))]
fn stop_stale_daemon(_port: u16) {}

fn connect_or_start(port: u16) -> io::Result<TcpStream> {
    stop_stale_daemon(port);
    if let Ok(stream) = connect(port) {
        return Ok(stream);
    }
    start_daemon(port)?;
    let started = Instant::now();
    loop {
        match connect(port) {
            Ok(stream) => return Ok(stream),
            Err(error) if started.elapsed() > START_TIMEOUT => return Err(error),
            Err(_) => thread::sleep(RETRY_INTERVAL),
        }
    }
}

fn pump(mut from: impl Read, mut to: impl Write) -> io::Result<()> {
    let mut buffer = [0u8; 16 * 1024];
    loop {
        let count = from.read(&mut buffer)?;
        if count == 0 {
            return Ok(());
        }
        to.write_all(&buffer[..count])?;
        to.flush()?;
    }
}

pub fn run(port: u16) -> io::Result<()> {
    let stream = connect_or_start(port)?;
    stream.set_nodelay(true)?;
    let inbound = stream.try_clone()?;
    let closer = stream.try_clone()?;
    thread::spawn(move || {
        let _ = pump(io::stdin().lock(), inbound);
        let _ = closer.shutdown(Shutdown::Write);
    });
    pump(stream, io::stdout().lock())
}
