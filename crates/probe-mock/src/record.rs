//! Capture a session against any probe and replay it without hardware.

use std::collections::VecDeque;
use std::io::{BufRead, Write};
use std::path::Path;
use std::time::Duration;

use probe_api::{
    AccessWidth, BpHandle, BpKind, ConnectCfg, DownloadStats, HssApi, Probe, ProbeError, ProbeInfo,
    Progress, Reg, ResetKind, Result, RttApi, SwoApi, TargetInfo, WatchpointSpec, WpHandle,
};
use serde::{Deserialize, Serialize};

/// One recorded operation and its outcome. An `Err` is stored as its message.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Op {
    Connect {
        result: std::result::Result<u32, String>,
    },
    Disconnect,
    Halt {
        result: Unit,
    },
    Go {
        result: Unit,
    },
    Step {
        result: Unit,
    },
    Reset {
        halt_after: bool,
        result: Unit,
    },
    IsHalted {
        result: std::result::Result<bool, String>,
    },
    ReadRegs {
        ids: Vec<Reg>,
        result: std::result::Result<Vec<u32>, String>,
    },
    WriteReg {
        id: Reg,
        value: u32,
        result: Unit,
    },
    ReadMem {
        addr: u32,
        len: usize,
        result: std::result::Result<Vec<u8>, String>,
    },
    WriteMem {
        addr: u32,
        data: Vec<u8>,
        result: Unit,
    },
    SetBp {
        addr: u32,
        result: std::result::Result<u32, String>,
    },
    ClearBp {
        handle: u32,
        result: Unit,
    },
    SetWp {
        addr: u32,
        result: std::result::Result<u32, String>,
    },
    ClearWp {
        handle: u32,
        result: Unit,
    },
}

type Unit = std::result::Result<(), String>;

fn stored<T>(result: &Result<T>) -> std::result::Result<(), String> {
    result.as_ref().map(|_| ()).map_err(ToString::to_string)
}

/// Passes every call through to `inner` and logs it.
pub struct RecordingProbe<P> {
    inner: P,
    ops: Vec<Op>,
}

impl<P: Probe> RecordingProbe<P> {
    pub fn new(inner: P) -> Self {
        Self {
            inner,
            ops: Vec::new(),
        }
    }

    pub fn ops(&self) -> &[Op] {
        &self.ops
    }

    /// Write the session as one JSON object per line.
    pub fn save(&self, writer: &mut dyn Write) -> std::io::Result<()> {
        for op in &self.ops {
            serde_json::to_writer(&mut *writer, op)?;
            writer.write_all(b"\n")?;
        }
        Ok(())
    }
}

impl<P: Probe> Probe for RecordingProbe<P> {
    fn info(&self) -> ProbeInfo {
        self.inner.info()
    }

    fn connect(&mut self, cfg: &ConnectCfg) -> Result<TargetInfo> {
        let result = self.inner.connect(cfg);
        self.ops.push(Op::Connect {
            result: result
                .as_ref()
                .map(|t| t.cpuid)
                .map_err(ToString::to_string),
        });
        result
    }

    fn disconnect(&mut self) -> Result<()> {
        self.ops.push(Op::Disconnect);
        self.inner.disconnect()
    }

    fn halt(&mut self) -> Result<()> {
        let result = self.inner.halt();
        self.ops.push(Op::Halt {
            result: stored(&result),
        });
        result
    }

    fn go(&mut self) -> Result<()> {
        let result = self.inner.go();
        self.ops.push(Op::Go {
            result: stored(&result),
        });
        result
    }

    fn step_instr(&mut self) -> Result<()> {
        let result = self.inner.step_instr();
        self.ops.push(Op::Step {
            result: stored(&result),
        });
        result
    }

    fn reset(&mut self, kind: ResetKind, halt_after: bool) -> Result<()> {
        let result = self.inner.reset(kind, halt_after);
        self.ops.push(Op::Reset {
            halt_after,
            result: stored(&result),
        });
        result
    }

    fn is_halted(&mut self) -> Result<bool> {
        let result = self.inner.is_halted();
        self.ops.push(Op::IsHalted {
            result: result.as_ref().copied().map_err(ToString::to_string),
        });
        result
    }

    fn read_regs(&mut self, ids: &[Reg], out: &mut [u32]) -> Result<()> {
        let result = self.inner.read_regs(ids, out);
        self.ops.push(Op::ReadRegs {
            ids: ids.to_vec(),
            result: stored(&result).map(|()| out.to_vec()),
        });
        result
    }

    fn write_reg(&mut self, id: Reg, v: u32) -> Result<()> {
        let result = self.inner.write_reg(id, v);
        self.ops.push(Op::WriteReg {
            id,
            value: v,
            result: stored(&result),
        });
        result
    }

    fn read_mem(&mut self, addr: u32, buf: &mut [u8], width: AccessWidth) -> Result<()> {
        let result = self.inner.read_mem(addr, buf, width);
        self.ops.push(Op::ReadMem {
            addr,
            len: buf.len(),
            result: stored(&result).map(|()| buf.to_vec()),
        });
        result
    }

    fn write_mem(&mut self, addr: u32, data: &[u8], width: AccessWidth) -> Result<()> {
        let result = self.inner.write_mem(addr, data, width);
        self.ops.push(Op::WriteMem {
            addr,
            data: data.to_vec(),
            result: stored(&result),
        });
        result
    }

    fn set_bp(&mut self, addr: u32, kind: BpKind) -> Result<BpHandle> {
        let result = self.inner.set_bp(addr, kind);
        self.ops.push(Op::SetBp {
            addr,
            result: result.as_ref().map(|h| h.0).map_err(ToString::to_string),
        });
        result
    }

    fn clear_bp(&mut self, h: BpHandle) -> Result<()> {
        let result = self.inner.clear_bp(h);
        self.ops.push(Op::ClearBp {
            handle: h.0,
            result: stored(&result),
        });
        result
    }

    fn set_wp(&mut self, w: &WatchpointSpec) -> Result<WpHandle> {
        let result = self.inner.set_wp(w);
        self.ops.push(Op::SetWp {
            addr: w.addr,
            result: result.as_ref().map(|h| h.0).map_err(ToString::to_string),
        });
        result
    }

    fn clear_wp(&mut self, h: WpHandle) -> Result<()> {
        let result = self.inner.clear_wp(h);
        self.ops.push(Op::ClearWp {
            handle: h.0,
            result: stored(&result),
        });
        result
    }

    fn download(
        &mut self,
        elf_or_bin: &Path,
        progress: &dyn Fn(Progress),
    ) -> Result<DownloadStats> {
        self.inner.download(elf_or_bin, progress)
    }

    fn hss(&mut self) -> Option<&mut dyn HssApi> {
        self.inner.hss()
    }

    fn rtt(&mut self) -> Option<&mut dyn RttApi> {
        self.inner.rtt()
    }

    fn swo(&mut self) -> Option<&mut dyn SwoApi> {
        None
    }
}

/// Answers calls from a recording, failing on any divergence from it.
pub struct ReplayProbe {
    ops: VecDeque<Op>,
}

impl ReplayProbe {
    pub fn new(ops: impl IntoIterator<Item = Op>) -> Self {
        Self {
            ops: ops.into_iter().collect(),
        }
    }

    pub fn load(reader: &mut dyn BufRead) -> std::io::Result<Self> {
        let mut ops = VecDeque::new();
        for line in reader.lines() {
            ops.push_back(serde_json::from_str(&line?)?);
        }
        Ok(Self { ops })
    }

    pub fn remaining(&self) -> usize {
        self.ops.len()
    }

    fn next(&mut self, expected: &str) -> Result<Op> {
        self.ops.pop_front().ok_or_else(|| {
            ProbeError::Other(format!("replay exhausted, but the code called {expected}"))
        })
    }

    fn diverged<T>(expected: &str, found: &Op) -> Result<T> {
        Err(ProbeError::Other(format!(
            "replay diverged: the code called {expected}, the recording has {found:?}"
        )))
    }
}

fn restore<T>(result: std::result::Result<T, String>) -> Result<T> {
    result.map_err(ProbeError::Other)
}

impl Probe for ReplayProbe {
    fn info(&self) -> ProbeInfo {
        ProbeInfo {
            product: "Replay".to_owned(),
            ..ProbeInfo::default()
        }
    }

    fn connect(&mut self, cfg: &ConnectCfg) -> Result<TargetInfo> {
        match self.next("connect")? {
            Op::Connect { result } => Ok(TargetInfo {
                device: cfg.device.clone(),
                speed_khz: cfg.speed_khz,
                cpuid: restore(result)?,
                hw_breakpoints: 8,
                halted: false,
            }),
            other => Self::diverged("connect", &other),
        }
    }

    fn disconnect(&mut self) -> Result<()> {
        match self.next("disconnect")? {
            Op::Disconnect => Ok(()),
            other => Self::diverged("disconnect", &other),
        }
    }

    fn halt(&mut self) -> Result<()> {
        match self.next("halt")? {
            Op::Halt { result } => restore(result),
            other => Self::diverged("halt", &other),
        }
    }

    fn go(&mut self) -> Result<()> {
        match self.next("go")? {
            Op::Go { result } => restore(result),
            other => Self::diverged("go", &other),
        }
    }

    fn step_instr(&mut self) -> Result<()> {
        match self.next("step_instr")? {
            Op::Step { result } => restore(result),
            other => Self::diverged("step_instr", &other),
        }
    }

    fn reset(&mut self, _kind: ResetKind, halt_after: bool) -> Result<()> {
        match self.next("reset")? {
            Op::Reset {
                halt_after: recorded,
                result,
            } if recorded == halt_after => restore(result),
            other => Self::diverged("reset", &other),
        }
    }

    fn is_halted(&mut self) -> Result<bool> {
        match self.next("is_halted")? {
            Op::IsHalted { result } => restore(result),
            other => Self::diverged("is_halted", &other),
        }
    }

    fn read_regs(&mut self, ids: &[Reg], out: &mut [u32]) -> Result<()> {
        match self.next("read_regs")? {
            Op::ReadRegs {
                ids: recorded,
                result,
            } if recorded == ids => {
                out.copy_from_slice(&restore(result)?);
                Ok(())
            }
            other => Self::diverged("read_regs", &other),
        }
    }

    fn write_reg(&mut self, id: Reg, v: u32) -> Result<()> {
        match self.next("write_reg")? {
            Op::WriteReg {
                id: rid,
                value,
                result,
            } if rid == id && value == v => restore(result),
            other => Self::diverged("write_reg", &other),
        }
    }

    fn read_mem(&mut self, addr: u32, buf: &mut [u8], _width: AccessWidth) -> Result<()> {
        match self.next("read_mem")? {
            Op::ReadMem {
                addr: raddr,
                len,
                result,
            } if raddr == addr && len == buf.len() => {
                buf.copy_from_slice(&restore(result)?);
                Ok(())
            }
            other => Self::diverged("read_mem", &other),
        }
    }

    fn write_mem(&mut self, addr: u32, data: &[u8], _width: AccessWidth) -> Result<()> {
        match self.next("write_mem")? {
            Op::WriteMem {
                addr: raddr,
                data: rdata,
                result,
            } if raddr == addr && rdata == data => restore(result),
            other => Self::diverged("write_mem", &other),
        }
    }

    fn set_bp(&mut self, addr: u32, _kind: BpKind) -> Result<BpHandle> {
        match self.next("set_bp")? {
            Op::SetBp {
                addr: raddr,
                result,
            } if raddr == addr => restore(result).map(BpHandle),
            other => Self::diverged("set_bp", &other),
        }
    }

    fn clear_bp(&mut self, h: BpHandle) -> Result<()> {
        match self.next("clear_bp")? {
            Op::ClearBp { handle, result } if handle == h.0 => restore(result),
            other => Self::diverged("clear_bp", &other),
        }
    }

    fn set_wp(&mut self, w: &WatchpointSpec) -> Result<WpHandle> {
        match self.next("set_wp")? {
            Op::SetWp { addr, result } if addr == w.addr => restore(result).map(WpHandle),
            other => Self::diverged("set_wp", &other),
        }
    }

    fn clear_wp(&mut self, h: WpHandle) -> Result<()> {
        match self.next("clear_wp")? {
            Op::ClearWp { handle, result } if handle == h.0 => restore(result),
            other => Self::diverged("clear_wp", &other),
        }
    }

    fn download(&mut self, _: &Path, _: &dyn Fn(Progress)) -> Result<DownloadStats> {
        Ok(DownloadStats {
            elapsed: Duration::ZERO,
        })
    }

    fn hss(&mut self) -> Option<&mut dyn HssApi> {
        None
    }

    fn rtt(&mut self) -> Option<&mut dyn RttApi> {
        None
    }

    fn swo(&mut self) -> Option<&mut dyn SwoApi> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MockProbe;
    use probe_api::ProbeExt;

    fn recording() -> RecordingProbe<MockProbe> {
        let mut mock = MockProbe::new();
        mock.add_region(0x2000_0000, vec![0; 16]);
        RecordingProbe::new(mock)
    }

    #[test]
    fn a_session_replays_through_a_file() {
        let mut probe = recording();
        probe.write_u32(0x2000_0000, 42).unwrap();
        probe.halt().unwrap();
        assert_eq!(probe.read_u32(0x2000_0000).unwrap(), 42);
        assert!(probe.read_u32(0x3000_0000).is_err());

        let mut bytes = Vec::new();
        probe.save(&mut bytes).unwrap();
        let mut replay = ReplayProbe::load(&mut bytes.as_slice()).unwrap();

        replay.write_u32(0x2000_0000, 42).unwrap();
        replay.halt().unwrap();
        assert_eq!(replay.read_u32(0x2000_0000).unwrap(), 42);
        let error = replay.read_u32(0x3000_0000).unwrap_err();
        assert!(error.to_string().contains("unmapped"), "{error}");
        assert_eq!(replay.remaining(), 0);
    }

    #[test]
    fn replay_rejects_a_different_call_sequence() {
        let mut probe = recording();
        probe.halt().unwrap();
        let mut replay = ReplayProbe::new(probe.ops().to_vec());
        let error = replay.go().unwrap_err();
        assert!(error.to_string().contains("diverged"), "{error}");
    }

    #[test]
    fn replay_reports_exhaustion() {
        let mut replay = ReplayProbe::new([]);
        assert!(replay.halt().unwrap_err().to_string().contains("exhausted"));
    }
}
