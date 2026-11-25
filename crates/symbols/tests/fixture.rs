//! End-to-end tests against a real GCC -O2 -g DWARF 5 image.
#![cfg(has_arm_gcc)]

use std::path::{Path, PathBuf};

use probe_api::{AccessWidth, Probe};
use probe_mock::MockProbe;
use symbols::{
    CoreRegisters, FormatOptions, FrameKind, FrameRegisters, LoadOptions, Located, MemoryReader,
    SymbolDb, UnwindStopReason,
};

const FIXTURE: &str = env!("FIXTURE_ELF");
const DATA_BASE: u32 = 0x2000_0000;

struct ProbeMemory(MockProbe);

impl MemoryReader for ProbeMemory {
    fn read(&mut self, addr: u32, buf: &mut [u8]) -> Result<(), String> {
        self.0
            .read_mem(addr, buf, AccessWidth::Auto)
            .map_err(|e| e.to_string())
    }
}

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("haltd-fixture-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

fn load() -> SymbolDb {
    SymbolDb::load(
        Path::new(FIXTURE),
        &LoadOptions {
            no_cache: true,
            ..LoadOptions::default()
        },
    )
    .unwrap()
}

/// A target holding the image's initialized data and read-only data.
fn target() -> ProbeMemory {
    let elf = std::fs::read(FIXTURE).unwrap();
    let (_, data) = section(&elf, ".data");
    let (rodata_addr, rodata) = section(&elf, ".rodata");
    let mut mock = MockProbe::new();
    let mut ram = vec![0u8; 0x4000];
    ram[..data.len()].copy_from_slice(&data);
    mock.add_region(DATA_BASE, ram);
    mock.add_region(rodata_addr, rodata);
    ProbeMemory(mock)
}

/// Load address and contents of an ELF32 section.
fn section(elf: &[u8], name: &str) -> (u32, Vec<u8>) {
    let le32 = |at: usize| u32::from_le_bytes(elf[at..at + 4].try_into().unwrap()) as usize;
    let le16 = |at: usize| u16::from_le_bytes(elf[at..at + 2].try_into().unwrap()) as usize;
    let (shoff, shentsize, shnum, shstrndx) = (le32(32), le16(46), le16(48), le16(50));
    let header = |i: usize| shoff + i * shentsize;
    let strings = le32(header(shstrndx) + 16);
    for i in 0..shnum {
        let start = strings + le32(header(i));
        let end = start + elf[start..].iter().position(|b| *b == 0).unwrap();
        if &elf[start..end] == name.as_bytes() {
            let offset = le32(header(i) + 16);
            let contents = elf[offset..offset + le32(header(i) + 20)].to_vec();
            return (le32(header(i) + 12) as u32, contents);
        }
    }
    panic!("no section {name}");
}

fn value(db: &SymbolDb, memory: &mut ProbeMemory, expression: &str) -> String {
    db.read_expression(expression, memory, &FormatOptions::default())
        .unwrap_or_else(|e| panic!("{expression}: {e}"))
        .value
}

#[test]
fn reads_scalars_through_paths() {
    let db = load();
    let mut m = target();
    assert_eq!(value(&db, &mut m, "g_counter"), "51966");
    assert_eq!(value(&db, &mut m, "fw::g_state.base_id"), "287454020");
    assert_eq!(value(&db, &mut m, "fw::g_state.items[0].a"), "-5");
    assert_eq!(value(&db, &mut m, "fw::g_state.items[2].gain"), "-0.25");
    assert_eq!(value(&db, &mut m, "fw::g_state.matrix[1][2]"), "6");
    assert_eq!(value(&db, &mut m, "fw::g_buffer.data[3]"), "3.5");
    assert_eq!(value(&db, &mut m, "fw::g_buffer.size"), "4");
}

#[test]
fn reads_bitfields_with_sign() {
    let db = load();
    let mut m = target();
    assert_eq!(value(&db, &mut m, "fw::g_state.items[0].enabled"), "1");
    assert_eq!(value(&db, &mut m, "fw::g_state.items[0].level"), "-2");
    assert_eq!(value(&db, &mut m, "fw::g_state.items[1].level"), "3");
    assert_eq!(value(&db, &mut m, "fw::g_state.items[1].enabled"), "0");
}

#[test]
fn follows_pointers_and_references() {
    let db = load();
    let mut m = target();
    assert_eq!(value(&db, &mut m, "g_state_ptr->mode"), "Run");
    assert_eq!(value(&db, &mut m, "(*g_state_ptr).mode"), "Run");
    assert_eq!(value(&db, &mut m, "g_state_ptr->cursor->gain"), "2.5");
    assert_eq!(value(&db, &mut m, "g_state_ptr->cursor[1].a"), "7");
    assert_eq!(value(&db, &mut m, "g_state_ref.items[1].a"), "6");
    // A variable tree names a reference's target as `*ref`.
    assert_eq!(value(&db, &mut m, "(*g_state_ref).items[1].a"), "6");
    assert_eq!(value(&db, &mut m, "(*g_state_ref).cursor->gain"), "2.5");
}

#[test]
fn finds_anonymous_union_members_and_bases() {
    let db = load();
    let mut m = target();
    assert_eq!(value(&db, &mut m, "fw::g_state.word"), "2882343476");
    assert_eq!(value(&db, &mut m, "g_state_ptr->base_id"), "287454020");
}

#[test]
fn resolves_scoped_statics() {
    let db = load();
    let mut m = target();
    assert_eq!(value(&db, &mut m, "fw::Widget::s_count"), "77");
    assert_eq!(value(&db, &mut m, "fw::hidden"), "5");
    assert_eq!(value(&db, &mut m, "bump::s_calls"), "11");
}

#[test]
fn accepts_unqualified_names_when_unique() {
    let db = load();
    let mut m = target();
    assert_eq!(value(&db, &mut m, "g_state.mode"), "Run");
    assert_eq!(value(&db, &mut m, "s_count"), "77");
}

#[test]
fn renders_aggregates() {
    let db = load();
    let mut m = target();
    assert_eq!(value(&db, &mut m, "fw::g_state.name"), "\"arm\"");
    assert_eq!(
        value(&db, &mut m, "fw::g_state.matrix"),
        "[[1, 2, 3], [4, 5, 6]]"
    );
    assert_eq!(
        value(&db, &mut m, "fw::g_state.items[0]"),
        "{a = -5, enabled = 1, level = -2, gain = 1.5}"
    );
    assert_eq!(
        value(&db, &mut m, "fw::g_buffer"),
        "{data = [0.5, 1.5, 2.5, 3.5], size = 4}"
    );
    let whole = value(&db, &mut m, "fw::g_state");
    assert!(
        whole.starts_with("{<Base> = {base_id = 287454020}, items = ["),
        "{whole}"
    );
    assert!(whole.contains("mode = Run"), "{whole}");
}

#[test]
fn takes_addresses() {
    let db = load();
    let mut m = target();
    let direct = db
        .read_expression("fw::g_state.items[1]", &mut m, &FormatOptions::default())
        .unwrap();
    let address = value(&db, &mut m, "&fw::g_state.items[1]");
    assert_eq!(address, format!("{:#010x}", direct.addr.unwrap()));
    assert_eq!(value(&db, &mut m, "*&fw::g_state.mode"), "Run");
}

#[test]
fn completes_forward_declarations_from_another_unit() {
    let db = load();
    let mut m = target();
    assert_eq!(value(&db, &mut m, "g_opaque->hidden_value"), "23130");
}

#[test]
fn reports_precise_errors() {
    let db = load();
    let mut m = target();
    let mut message = |expression: &str| {
        db.read_expression(expression, &mut m, &FormatOptions::default())
            .unwrap_err()
            .to_string()
    };
    assert!(message("no_such_symbol").contains("no symbol"));
    assert!(message("fw::g_state.nothing").contains("no member"));
    assert!(message("fw::g_state.items[3]").contains("out of range"));
    assert!(message("fw::g_state.mode->x").contains("not a pointer"));
    assert!(message("&fw::g_state.items[0].level").contains("bit-field"));
}

#[test]
fn indexes_functions_and_elf_symbols() {
    let db = load();
    let bump = db.functions("bump");
    assert_eq!(bump.len(), 1);
    assert!(bump[0].low >= 0x0800_0000);
    let found = db.function_at(bump[0].low).unwrap();
    assert_eq!(found.name, "bump");
    assert!(db.function_at(0x0700_0000).is_none());

    let (addr, size) = db.elf_symbol("g_counter").unwrap();
    assert!(addr >= DATA_BASE);
    assert_eq!(size, 4);
    let counter = db.globals("g_counter");
    assert_eq!((counter[0].addr, counter[0].size), (addr, 4));
}

#[test]
fn excludes_objects_discarded_by_the_linker() {
    let db = load();
    assert!(db.globals("g_discarded").is_empty());
}

#[test]
fn indexes_types_and_source_files() {
    let db = load();
    assert!(db.type_by_name("fw::Inner").is_some());
    assert!(db.type_by_name("fw::Buffer<float, 4>").is_some());
    assert!(db.type_by_name("Opaque").is_some());

    let files = db.source_files("tests/fixtures/basic.cpp");
    assert_eq!(files.len(), 1, "{files:?}");
    assert!(std::path::Path::new(&files[0]).is_absolute(), "{files:?}");
    // A partial path component must not match.
    assert!(db.source_files("ixtures/basic.cpp").is_empty());
}

#[test]
fn cache_hit_gives_identical_answers() {
    let dir = scratch("cache");
    let options = LoadOptions {
        cache_dir: Some(dir),
        no_cache: false,
    };
    let first = SymbolDb::load(Path::new(FIXTURE), &options).unwrap();
    assert!(!first.stats().cache_hit);
    let second = SymbolDb::load(Path::new(FIXTURE), &options).unwrap();
    assert!(second.stats().cache_hit);
    assert_eq!(first.hash(), second.hash());
    assert_eq!(first.stats().globals, second.stats().globals);

    let mut m = target();
    assert_eq!(value(&second, &mut m, "fw::g_state.items[2].gain"), "-0.25");
}

const STACK: u32 = DATA_BASE + 0x2000;

fn write_words(m: &mut ProbeMemory, addr: u32, words: &[u32]) {
    for (index, word) in words.iter().enumerate() {
        m.0.write_mem(
            addr + index as u32 * 4,
            &word.to_le_bytes(),
            AccessWidth::Auto,
        )
        .unwrap();
    }
}

fn names(frames: &[symbols::StackFrame]) -> Vec<String> {
    frames
        .iter()
        .map(|frame| match frame.kind {
            FrameKind::Exception {
                number,
                heuristic: false,
            } => format!("<exception {number}>"),
            FrameKind::Exception {
                number,
                heuristic: true,
            } => format!("<exception {number} (heuristic)>"),
            FrameKind::Code => frame.function.clone().unwrap_or_default(),
        })
        .collect()
}

/// PC on the final `pop`, where the pushed registers are still on the stack.
fn in_with_frame(db: &SymbolDb) -> u32 {
    db.functions("with_frame")[0].high - 2
}

fn return_into_entry(db: &SymbolDb) -> u32 {
    (db.functions("entry")[0].low + 4) | 1
}

#[test]
fn unwinds_through_a_pushed_return_address() {
    let db = load();
    let mut m = target();
    write_words(&mut m, STACK, &[0, return_into_entry(&db)]);
    let mut start = CoreRegisters::default();
    start.r[13] = STACK;
    start.r[15] = in_with_frame(&db);

    let frames = db.unwind(&start, &mut m).unwrap();
    assert_eq!(names(&frames), ["with_frame", "entry"]);
    assert_eq!(frames[1].sp, STACK + 8);
}

#[test]
fn unwinds_through_an_exception_on_the_main_stack() {
    let db = load();
    let mut m = target();
    let interrupted_pc = in_with_frame(&db);
    write_words(
        &mut m,
        STACK,
        &[0, 0, 0, 0, 0, 0xBAD, interrupted_pc, 0x0100_0000],
    );
    write_words(&mut m, STACK + 0x20, &[0, return_into_entry(&db)]);
    let mut start = CoreRegisters {
        xpsr: 15,
        msp: STACK,
        ..CoreRegisters::default()
    };
    start.r[13] = STACK;
    start.r[14] = 0xFFFF_FFF9;
    start.r[15] = db.functions("bump")[0].low;

    let frames = db.unwind(&start, &mut m).unwrap();
    assert_eq!(
        names(&frames),
        ["bump", "<exception 15>", "with_frame", "entry"]
    );
}

#[test]
fn takes_the_exception_frame_from_the_process_stack_with_padding() {
    let db = load();
    let mut m = target();
    let process_stack = STACK + 0x400;
    write_words(
        &mut m,
        process_stack,
        &[0, 0, 0, 0, 0, 0, in_with_frame(&db), 0x0100_0200],
    );
    write_words(&mut m, process_stack + 0x24, &[0, return_into_entry(&db)]);
    let mut start = CoreRegisters {
        xpsr: 11,
        msp: STACK,
        psp: process_stack,
        ..CoreRegisters::default()
    };
    start.r[13] = STACK;
    start.r[14] = 0xFFFF_FFFD;
    start.r[15] = db.functions("bump")[0].low;

    let frames = db.unwind(&start, &mut m).unwrap();
    assert_eq!(
        names(&frames),
        ["bump", "<exception 11>", "with_frame", "entry"]
    );
}

#[test]
fn a_memory_fault_ends_the_walk_with_what_was_found() {
    let db = load();
    let mut m = target();
    let mut start = CoreRegisters::default();
    start.r[13] = 0x1000_0000;
    start.r[15] = in_with_frame(&db);
    let frames = db.unwind(&start, &mut m).unwrap();
    assert_eq!(names(&frames), ["with_frame"]);
}

#[test]
fn stops_at_the_task_exit_stub_planted_as_a_return_address() {
    let db = load();
    let mut m = target();
    let stub = db.functions("prvTaskExitError")[0].low;
    write_words(&mut m, STACK, &[0, stub | 1]);
    let mut start = CoreRegisters::default();
    start.r[13] = STACK;
    start.r[15] = in_with_frame(&db);
    let (frames, reason) = db.unwind_detailed(&start, &mut m, 64).unwrap();
    assert_eq!(names(&frames), ["with_frame", "prvTaskExitError"]);
    assert_eq!(reason, UnwindStopReason::Bottom);
}

#[test]
fn unwinds_naked_pendsv_handler_to_interrupted_task_via_psp_heuristic() {
    let db = load();
    let mut m = target();
    let (pendsv_addr, _) = db.elf_symbol("sim_pendsv_handler").unwrap();
    let switch_fn = db.functions("c_switch_context")[0].clone();
    let stub = db.functions("prvTaskExitError")[0].low;
    // Find the instruction right after `bl c_switch_context` inside sim_pendsv_handler.
    // Prologue: mrs (4), stmdb r0! (4), stmdb sp! (4), bl (4) -> return offset is +16.
    let return_in_pendsv = (pendsv_addr + 16) | 1;

    let msp = STACK;
    let psp = STACK + 0x400;
    // MSP has {r0, r3} pushed by sim_pendsv_handler (no LR pushed on MSP!)
    write_words(&mut m, msp, &[psp - 9 * 4, 0]);
    // Task stack below PSP has FreeRTOS {r4-r11, EXC_RETURN}
    let mut below: Vec<u32> = (4..=11).collect();
    below.push(0xFFFF_FFFD);
    write_words(&mut m, psp - 9 * 4, &below);
    // Hardware exception frame at PSP interrupted inside `with_frame`
    write_words(
        &mut m,
        psp,
        &[0, 0, 0, 0, 0, 0, in_with_frame(&db), 0x0100_0000],
    );
    // `with_frame` pushed {r3, lr = prvTaskExitError}
    write_words(&mut m, psp + 0x20, &[0, stub | 1]);

    let mut start = CoreRegisters {
        xpsr: 14,
        msp,
        psp,
        ..CoreRegisters::default()
    };
    start.r[13] = msp;
    start.r[14] = return_in_pendsv;
    start.r[15] = switch_fn.low;

    let (frames, reason) = db.unwind_detailed(&start, &mut m, 64).unwrap();
    assert_eq!(
        names(&frames),
        [
            "c_switch_context",
            "sim_pendsv_handler",
            "<exception 14 (heuristic)>",
            "with_frame",
            "prvTaskExitError",
        ]
    );
    assert_eq!(reason, UnwindStopReason::Bottom);
}

#[test]
fn unwinds_empty_fde_naked_caller_via_thumb_prologue_scanner() {
    let db = load();
    let mut m = target();
    let (naked_addr, _) = db.elf_symbol("sim_naked_caller").unwrap();
    let switch_fn = db.functions("c_switch_context")[0].clone();
    let stub = db.functions("prvTaskExitError")[0].low;
    // Prologue: push {r4, lr} (2B), sub sp, #8 (2B), bl c_switch_context (4B) -> +8.
    let return_in_naked = (naked_addr + 8) | 1;

    // Frame layout inside sim_naked_caller: [sp+0..8]=local, [sp+8]=r4, [sp+12]=lr.
    write_words(&mut m, STACK, &[0, 0, 0x4444_4444, stub | 1]);
    let mut start = CoreRegisters::default();
    start.r[13] = STACK;
    start.r[14] = return_in_naked;
    start.r[15] = switch_fn.low;

    let (frames, reason) = db.unwind_detailed(&start, &mut m, 64).unwrap();
    assert_eq!(
        names(&frames),
        ["c_switch_context", "sim_naked_caller", "prvTaskExitError"]
    );
    assert_eq!(reason, UnwindStopReason::Bottom);
}

fn marker_line(marker: &str) -> u32 {
    let source = include_str!("fixtures/basic.cpp");
    let needle = format!("// LINE:{marker}");
    source
        .lines()
        .position(|line| line.contains(&needle))
        .unwrap_or_else(|| panic!("no marker {marker}")) as u32
        + 1
}

const SOURCE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/basic.cpp");

#[test]
fn lines_and_addresses_map_both_ways() {
    let db = load();
    let line = marker_line("before_inline");
    let resolved = db.resolve_line(SOURCE, line).unwrap().unwrap();
    assert!(resolved.file.ends_with("fixtures/basic.cpp"));
    assert!(!resolved.addresses.is_empty());
    let function = &db.functions("uses_inline")[0];
    let statements = db.statements_in(function.low, function.high).unwrap();
    for address in &resolved.addresses {
        // Optimized code can have several rows at one address, so look for
        // the line among them rather than expecting it to be the last.
        assert!(
            statements
                .iter()
                .any(|(at, location)| at == address && location.line == resolved.line),
            "{address:#x}"
        );
        assert!(db.is_statement_start(*address).unwrap());
    }
}

#[test]
fn a_build_tree_elsewhere_still_matches_by_path_tail() {
    let db = load();
    let line = marker_line("before_inline");
    let moved = db
        .resolve_line("/other/checkout/fixtures/basic.cpp", line)
        .unwrap();
    assert!(moved.is_some());
    assert!(db.resolve_line("/x/unrelated.cpp", line).unwrap().is_none());
}

#[test]
fn a_blank_line_slides_forward_to_the_next_code() {
    let db = load();
    let code = marker_line("after_gap");
    let resolved = db.resolve_line(SOURCE, code - 1).unwrap().unwrap();
    assert_eq!(resolved.line, code);
}

#[test]
fn no_code_within_ten_lines_is_unresolved() {
    let db = load();
    // The top of the file is only declarations for far more than ten lines.
    assert!(db.resolve_line(SOURCE, 1).unwrap().is_none());
}

/// Addresses inside `uses_inline` where `add_twice` is active.
fn inlined_addresses(db: &SymbolDb) -> Vec<u32> {
    let function = &db.functions("uses_inline")[0];
    (function.low..function.high)
        .step_by(2)
        .filter(|pc| db.scope_at(*pc).unwrap().len() == 2)
        .collect()
}

#[test]
fn reports_the_inline_chain_with_its_call_site() {
    let db = load();
    let addresses = inlined_addresses(&db);
    assert!(!addresses.is_empty());
    let chain = db.scope_at(addresses[0]).unwrap();
    assert_eq!(chain[0].name, "uses_inline");
    assert!(!chain[0].is_inlined);
    assert_eq!(chain[1].name, "add_twice");
    assert!(chain[1].is_inlined);
    let call = chain[1].call_site.as_ref().unwrap();
    assert_eq!(call.line, marker_line("inline_call"));
    assert!(chain[1].vars.iter().any(|var| var.name == "v"));
}

#[test]
fn code_without_debug_info_has_no_scope() {
    let db = load();
    assert!(db.scope_at(0x0700_0000).unwrap().is_empty());
}

fn regs_with(values: &[(usize, u32)]) -> [Option<u32>; 16] {
    let mut r = [Some(0); 16];
    for (index, value) in values {
        r[*index] = Some(*value);
    }
    r
}

fn locate_param(db: &SymbolDb, m: &mut ProbeMemory, pc: u32, r: &[Option<u32>; 16]) -> Located {
    let chain = db.scope_at(pc).unwrap();
    let var = chain[0].vars.iter().find(|v| v.name == "x").unwrap();
    let frame = FrameRegisters {
        pc,
        r,
        s: None,
        cfa: None,
    };
    db.locate(var, &frame, m).unwrap()
}

#[test]
fn location_lists_follow_the_pc() {
    let db = load();
    let mut m = target();
    let function = db.functions("with_frame")[0].clone();
    // On entry the argument is in R0.
    let at_entry = locate_param(&db, &mut m, function.low, &regs_with(&[(0, 7)]));
    assert_eq!(at_entry, Located::Bytes(7u32.to_le_bytes().to_vec()));
}

#[test]
fn an_unknown_register_is_optimized_out_not_garbage() {
    let db = load();
    let mut m = target();
    let function = db.functions("with_frame")[0].clone();
    let mut r = regs_with(&[]);
    r[0] = None;
    assert_eq!(
        locate_param(&db, &mut m, function.low, &r),
        Located::OptimizedOut
    );
}

#[test]
fn a_pc_outside_every_entry_is_optimized_out() {
    let db = load();
    let mut m = target();
    let function = db.functions("uses_inline")[0].clone();
    let chain = db.scope_at(function.low).unwrap();
    let var = chain[0].vars.iter().find(|v| v.name == "total").unwrap();
    let r = regs_with(&[]);
    let frame = FrameRegisters {
        pc: function.low.wrapping_sub(0x100),
        r: &r,
        s: None,
        cfa: None,
    };
    assert_eq!(
        db.locate(var, &frame, &mut m).unwrap(),
        Located::OptimizedOut
    );
}

#[test]
fn optimized_functions_stop_at_their_entry() {
    let db = load();
    let function = db.functions("uses_inline")[0].clone();
    assert!(db.is_optimized(function.low).unwrap());
    let after = db.after_prologue(function.low, function.high).unwrap();
    assert_eq!(after, function.low);
}

#[test]
fn completes_globals_and_encodes_enum_names() {
    let db = load();
    let mut m = target();
    let completions = db.complete_globals("g_state", 10);
    assert!(completions.iter().any(|c| c.name == "g_state_ptr"));

    let parsed = symbols::parse("g_state_ptr->mode").unwrap();
    let symbols::Evaluated::Place(place) = db.evaluate(&parsed, &mut m).unwrap() else {
        panic!("expected Place");
    };
    let run_val = db.encode_place_value(&place, "Run").unwrap();
    let current = db
        .render_place(&place, &mut m, &FormatOptions::default())
        .unwrap();
    assert_eq!(current, "Run");
    assert!(run_val <= 16);
}
