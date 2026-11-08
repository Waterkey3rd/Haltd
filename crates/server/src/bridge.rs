//! `jlinkd dap`: connects an editor's stdio to the daemon, starting it if
//! needed. Messages pass through unchanged.

use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::net::{Ipv4Addr, Shutdown, TcpStream};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

const START_TIMEOUT: Duration = Duration::from_secs(5);
const RETRY_INTERVAL: Duration = Duration::from_millis(50);

fn connect(port: u16) -> io::Result<TcpStream> {
    TcpStream::connect((Ipv4Addr::LOCALHOST, port))
}

fn log_file() -> Option<File> {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache")))?;
    let directory = base.join("jlinkd");
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
    Command::new(program)
        .args(["--log", "info", "serve", "--dap-port", &port.to_string()])
        .stdin(Stdio::null())
        .stdout(stdout)
        .stderr(stderr)
        .spawn()
        .map(|_| ())
}

fn connect_or_start(port: u16) -> io::Result<TcpStream> {
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
