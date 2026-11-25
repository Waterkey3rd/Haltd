//! Integration tests for dual-attach state sync, CoreSight DCRDR fallback,
//! hardware comparator detection, and unwind cache consistency.
#![cfg(has_arm_gcc)]

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use engine::breakpoints::FunctionBreakpoint;
use engine::conditions::BreakpointOptions;
use engine::project::ProjectConfig;
use engine::session::{HaltedPoll, LaunchOptions, RunState, Session, StopReason, MAIN_THREAD};
use probe_api::{
    AccessWidth, BpHandle, BpKind, CallStats, ConnectCfg, DownloadStats, HssApi, Interface, Probe,
    ProbeInfo, Progress, Reg, ResetKind, Result as ProbeResult, RttApi, SwoApi, TargetInfo,
    WatchpointSpec, WpHandle,
};
use probe_jlink::JLinkOptions;
use probe_mock::MockProbe;
use symbols::{LoadOptions, SymbolDb};

const SYNC_ELF: &str = env!("FIXTURE_SYNC");

#[derive(Clone)]
struct SharedMock(Arc<Mutex<MockProbe>>);

impl Probe for SharedMock {
    fn info(&self) -> ProbeInfo {
        self.0.lock().expect("mock lock").info()
    }

    fn connect(&mut self, cfg: &ConnectCfg) -> ProbeResult<TargetInfo> {
        self.0.lock().expect("mock lock").connect(cfg)
    }

    fn disconnect(&mut self) -> ProbeResult<()> {
        self.0.lock().expect("mock lock").disconnect()
    }

    fn halt(&mut self) -> ProbeResult<()> {
        self.0.lock().expect("mock lock").halt()
    }

    fn go(&mut self) -> ProbeResult<()> {
        self.0.lock().expect("mock lock").go()
    }

    fn step_instr(&mut self) -> ProbeResult<()> {
        self.0.lock().expect("mock lock").step_instr()
    }

    fn reset(&mut self, kind: ResetKind, halt_after: bool) -> ProbeResult<()> {
        self.0.lock().expect("mock lock").reset(kind, halt_after)
    }

    fn is_halted(&mut self) -> ProbeResult<bool> {
        self.0.lock().expect("mock lock").is_halted()
    }

    fn read_regs(&mut self, ids: &[Reg], out: &mut [u32]) -> ProbeResult<()> {
        self.0.lock().expect("mock lock").read_regs(ids, out)
    }

    fn write_reg(&mut self, id: Reg, v: u32) -> ProbeResult<()> {
        self.0.lock().expect("mock lock").write_reg(id, v)
    }

    fn read_mem(&mut self, addr: u32, buf: &mut [u8], width: AccessWidth) -> ProbeResult<()> {
        self.0.lock().expect("mock lock").read_mem(addr, buf, width)
    }

    fn write_mem(&mut self, addr: u32, data: &[u8], width: AccessWidth) -> ProbeResult<()> {
        self.0
            .lock()
            .expect("mock lock")
            .write_mem(addr, data, width)
    }

    fn set_bp(&mut self, addr: u32, kind: BpKind) -> ProbeResult<BpHandle> {
        self.0.lock().expect("mock lock").set_bp(addr, kind)
    }

    fn clear_bp(&mut self, h: BpHandle) -> ProbeResult<()> {
        self.0.lock().expect("mock lock").clear_bp(h)
    }

    fn set_wp(&mut self, w: &WatchpointSpec) -> ProbeResult<WpHandle> {
        self.0.lock().expect("mock lock").set_wp(w)
    }

    fn clear_wp(&mut self, h: WpHandle) -> ProbeResult<()> {
        self.0.lock().expect("mock lock").clear_wp(h)
    }

    fn download(
        &mut self,
        elf_or_bin: &Path,
        progress: &dyn Fn(Progress),
    ) -> ProbeResult<DownloadStats> {
        self.0
            .lock()
            .expect("mock lock")
            .download(elf_or_bin, progress)
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

    fn call_stats(&self) -> CallStats {
        self.0.lock().expect("mock lock").call_stats()
    }
}

fn launch_options() -> LaunchOptions {
    LaunchOptions {
        connect: ConnectCfg {
            device: "Cortex-M3".to_owned(),
            interface: Interface::Swd,
            speed_khz: 4000,
        },
        probe: JLinkOptions::default(),
        elf: PathBuf::from(SYNC_ELF),
        project: ProjectConfig::default(),
        workspace: None,
    }
}

fn setup_halted_at_outer(db: &SymbolDb) -> (Arc<Mutex<MockProbe>>, u32, u32) {
    let outer = db
        .functions("outer_a")
        .into_iter()
        .next()
        .expect("outer_a symbol");
    let inner = db
        .functions("inner_b")
        .into_iter()
        .next()
        .expect("inner_b symbol");

    let mut mock = MockProbe::new();
    // 16 KB RAM at 0x2000_0000 and DEMCR register (0xE000_EDFC) modified on connect.
    mock.add_region(0x2000_0000, vec![0; 0x4000]);
    mock.add_region(0xE000_EDFC, vec![0; 4]);
    mock.mock_external_halt(outer.low, 0);
    mock.set_reg(Reg::R(13), 0x2000_1F00);
    mock.set_reg(Reg::Msp, 0x2000_1F00);
    mock.set_reg(Reg::R(14), 0);
    mock.set_reg(Reg::Xpsr, 0x0100_0000);
    (Arc::new(Mutex::new(mock)), outer.low, inner.low)
}

#[test]
fn external_resume_and_halt_bypass_stale_jlink_register_cache() {
    let db = SymbolDb::load(Path::new(SYNC_ELF), &LoadOptions::default()).expect("load fixture");
    let (mock, outer_pc, inner_pc) = setup_halted_at_outer(&db);
    let mut session = Session::start(Box::new(SharedMock(mock.clone())), launch_options())
        .expect("start session");
    assert_eq!(*session.state(), RunState::Halted);

    // Freeze read_regs cache at outer_pc, then let external debugger resume target.
    {
        let mut m = mock.lock().expect("mock lock");
        m.mock_set_stale_read_regs(true);
        m.mock_external_resume();
    }
    assert_eq!(
        session.poll_halted().expect("poll_halted"),
        HaltedPoll::ResumedExternally
    );
    assert_eq!(*session.state(), RunState::Running);

    // Simulate external debugger halting inside inner_b called from outer_a,
    // while J-Link read_regs cache still reports outer_pc and lr=0.
    {
        let mut m = mock.lock().expect("mock lock");
        m.mock_external_halt(inner_pc, 0);
        m.mock_set_reg(Reg::R(13), 0x2000_1F00);
        m.mock_set_reg(Reg::Msp, 0x2000_1F00);
        m.mock_set_reg(Reg::R(14), (outer_pc + 8) | 1);
        m.mock_set_reg(Reg::Xpsr, 0x0100_0000);
    }

    let reason = session.poll().expect("poll running");
    assert_eq!(reason, Some(StopReason::External));
    assert_eq!(*session.state(), RunState::Halted);

    let frames = session.stack_trace(MAIN_THREAD).expect("stack trace");
    let names: Vec<&str> = frames.iter().map(|f| f.name.as_str()).collect();
    assert!(
        names.starts_with(&["inner_b", "outer_a"]),
        "expected CoreSight DCRDR refresh to unwind [inner_b, outer_a], got {names:?}"
    );

    session.shutdown().expect("shutdown");
}

#[test]
fn external_step_while_halted_emits_moved_and_refreshes_stack() {
    let db = SymbolDb::load(Path::new(SYNC_ELF), &LoadOptions::default()).expect("load fixture");
    let (mock, outer_pc, inner_pc) = setup_halted_at_outer(&db);
    let mut session = Session::start(Box::new(SharedMock(mock.clone())), launch_options())
        .expect("start session");

    let initial = session.stack_trace(MAIN_THREAD).expect("initial stack");
    assert_eq!(initial[0].name, "outer_a");

    // External debugger single-steps / moves into inner_b while probe stays halted.
    {
        let mut m = mock.lock().expect("mock lock");
        m.mock_set_stale_read_regs(true);
        m.mock_external_halt(inner_pc, 0);
        m.mock_set_reg(Reg::R(14), (outer_pc + 8) | 1);
    }

    assert_eq!(
        session.poll_halted().expect("poll halted move"),
        HaltedPoll::MovedExternally(StopReason::External)
    );
    let updated = session.stack_trace(MAIN_THREAD).expect("updated stack");
    let names: Vec<&str> = updated.iter().map(|f| f.name.as_str()).collect();
    assert!(
        names.starts_with(&["inner_b", "outer_a"]),
        "expected refreshed stack [inner_b, outer_a], got {names:?}"
    );

    session.shutdown().expect("shutdown");
}

#[test]
fn warns_when_preexisting_fpb_comparators_are_active() {
    let db = SymbolDb::load(Path::new(SYNC_ELF), &LoadOptions::default()).expect("load fixture");
    let (mock, _, _) = setup_halted_at_outer(&db);
    {
        let mut m = mock.lock().expect("mock lock");
        // FP_CTRL: ENABLE=1, NUM_CODE=1 -> 0x11; FP_COMP0: ENABLE=1 -> 0x1.
        let mut fpb = vec![0u8; 16];
        fpb[0..4].copy_from_slice(&0x0000_0011u32.to_le_bytes());
        fpb[8..12].copy_from_slice(&0x0000_0001u32.to_le_bytes());
        m.add_region(0xE000_2000, fpb);
    }

    let mut session =
        Session::start(Box::new(SharedMock(mock)), launch_options()).expect("start session");
    let output = session.take_output().join("\n");
    assert!(
        output.contains("pre-existing hardware comparators (1 FPB, 0 DWT)"),
        "expected FPB occupancy warning in output, got: {output:?}"
    );

    session.shutdown().expect("shutdown");
}

#[test]
fn conditional_breakpoint_shallow_unwind_does_not_truncate_later_stack_trace() {
    let db = SymbolDb::load(Path::new(SYNC_ELF), &LoadOptions::default()).expect("load fixture");
    let (mock, outer_pc, _inner_pc) = setup_halted_at_outer(&db);
    let mut session = Session::start(Box::new(SharedMock(mock.clone())), launch_options())
        .expect("start session");

    let resolved = session
        .set_function_breakpoints(&[FunctionBreakpoint {
            name: "inner_b".to_owned(),
            options: BreakpointOptions {
                condition: Some("g_cond == 0".to_owned()),
                hit_condition: None,
                log_message: None,
            },
        }])
        .expect("set conditional bp");
    assert!(
        resolved[0].verified,
        "breakpoint must resolve: {resolved:?}"
    );
    let bp_id = resolved[0].id;
    let bp_pc = {
        let mut m = mock.lock().expect("mock lock");
        let bps = m.breakpoints();
        assert!(!bps.is_empty(), "mock probe must hold breakpoint");
        m.push_halt_after_polls(1);
        bps[0].0
    };
    session.resume().expect("resume");
    {
        let mut m = mock.lock().expect("mock lock");
        m.set_reg(Reg::R(15), bp_pc);
        m.set_reg(Reg::R(14), (outer_pc + 8) | 1);
    }

    let stop = session.poll().expect("poll hit");
    assert_eq!(stop, Some(StopReason::Breakpoint(vec![bp_id])));

    let frames = session.stack_trace(MAIN_THREAD).expect("stack trace");
    let names: Vec<&str> = frames.iter().map(|f| f.name.as_str()).collect();
    assert!(
        names.starts_with(&["inner_b", "outer_a"]),
        "shallow_unwind must be reset so full stack is returned, got {names:?}"
    );

    session.shutdown().expect("shutdown");
}

#[test]
fn external_resume_ignores_stale_is_halted_flag_via_dhcsr() {
    let db = SymbolDb::load(Path::new(SYNC_ELF), &LoadOptions::default()).expect("load fixture");
    let (mock, _, _) = setup_halted_at_outer(&db);
    let mut session = Session::start(Box::new(SharedMock(mock.clone())), launch_options())
        .expect("start session");
    assert_eq!(*session.state(), RunState::Halted);

    {
        let mut m = mock.lock().expect("mock lock");
        m.mock_external_resume();
        m.mock_set_stale_is_halted(Some(true));
    }
    assert_eq!(
        session.poll_halted().expect("poll_halted"),
        HaltedPoll::ResumedExternally
    );
    assert_eq!(*session.state(), RunState::Running);

    // Even while J-Link is_halted() remains stale true, poll() must consult DHCSR.S_HALT
    // and NOT bounce back to Halted.
    assert_eq!(session.poll().expect("poll running"), None);
    assert_eq!(*session.state(), RunState::Running);

    session.shutdown().expect("shutdown");
}
