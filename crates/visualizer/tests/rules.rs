//! Built-in rules against real compiler output.
#![cfg(has_arm_gcc)]

use std::path::Path;

use probe_api::{AccessWidth, Probe};
use probe_mock::MockProbe;
use symbols::{LoadOptions, MemoryReader, Place, SymbolDb};
use visualizer::{Visualized, Visualizers};

const FIXTURE: &str = env!("FIXTURE_ELF");

struct Memory(MockProbe);

impl MemoryReader for Memory {
    fn read(&mut self, addr: u32, buf: &mut [u8]) -> Result<(), String> {
        self.0
            .read_mem(addr, buf, AccessWidth::Auto)
            .map_err(|e| e.to_string())
    }
}

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

struct Setup {
    db: SymbolDb,
    memory: Memory,
    rules: Visualizers,
}

fn setup() -> Setup {
    let db = SymbolDb::load(
        Path::new(FIXTURE),
        &LoadOptions {
            no_cache: true,
            ..LoadOptions::default()
        },
    )
    .unwrap();
    let elf = std::fs::read(FIXTURE).unwrap();
    let mut mock = MockProbe::new();
    let (address, data) = section(&elf, ".data");
    let mut ram = data;
    ram.resize(0x2000, 0);
    mock.add_region(address, ram);
    let (address, rodata) = section(&elf, ".rodata");
    mock.add_region(address, rodata);
    Setup {
        db,
        memory: Memory(mock),
        rules: Visualizers::with_builtin().unwrap(),
    }
}

impl Setup {
    fn place(&self, name: &str) -> Place {
        let variable = self.db.globals(name).into_iter().next().unwrap();
        Place::new(
            variable.addr,
            self.db.variable_type(&variable).unwrap().unwrap(),
        )
    }

    fn apply(&mut self, name: &str) -> Option<Visualized> {
        let place = self.place(name);
        self.rules.apply(&self.db, &mut self.memory, place)
    }

    fn summary(&mut self, name: &str) -> String {
        self.apply(name)
            .unwrap_or_else(|| panic!("{name}: no rule applied"))
            .summary
            .unwrap_or_else(|| panic!("{name}: no summary"))
    }

    fn child_names(&mut self, name: &str) -> Vec<String> {
        self.apply(name)
            .unwrap()
            .children
            .unwrap()
            .into_iter()
            .map(|(child, _)| child)
            .collect()
    }
}

#[test]
fn unit_shows_its_value() {
    let mut s = setup();
    assert_eq!(s.summary("g_unit"), "1.5 rad");
    assert!(s.child_names("g_unit").is_empty());
    assert_eq!(s.summary("g_torque"), "2 m^2·kg/s^2");
    assert_eq!(s.summary("g_mm"), "3 ×0.001 m");
}

#[test]
fn vectors_and_matrices_read_in_row_order() {
    let mut s = setup();
    assert_eq!(s.summary("g_vector"), "[1, 2, 3]");
    assert_eq!(s.child_names("g_vector"), ["[0]", "[1]", "[2]"]);
    assert_eq!(s.summary("g_matrix"), "[1, 3, 5; 2, 4, 6]");
    assert_eq!(s.summary("g_row_major"), "[1, 2, 3; 4, 5, 6]");
    assert_eq!(s.child_names("g_matrix")[1], "[0,1]");
}

#[test]
fn quaternion_puts_w_first() {
    let mut s = setup();
    assert_eq!(s.summary("g_quat"), "w=0.9 x=0.1 y=0.2 z=0.3");
    assert_eq!(s.child_names("g_quat"), ["w", "x", "y", "z"]);
}

#[test]
fn std_array_lists_elements() {
    let mut s = setup();
    assert_eq!(s.summary("g_array"), "[7, 8, 9]");
}

#[test]
fn optional_distinguishes_empty() {
    let mut s = setup();
    assert_eq!(s.summary("g_some"), "2.5");
    assert_eq!(s.child_names("g_some"), ["value"]);
    assert_eq!(s.summary("g_none"), "nullopt");
    assert!(s.child_names("g_none").is_empty());
}

#[test]
fn expressions_reach_what_rules_show() {
    let mut s = setup();
    let base = |s: &Setup, name: &str| s.db.globals(name)[0].addr;
    let mut address = |text: &str| {
        let expr = symbols::parse(text).unwrap();
        match s.db.evaluate_virtual(&expr, &mut s.memory, Some(&s.rules)) {
            Ok(symbols::Evaluated::Place(place)) => Ok(place.addr),
            Ok(other) => panic!("{text}: {other:?}"),
            Err(error) => Err(error.to_string()),
        }
    };
    let results = [
        address("g_array[1]"),
        address("g_some.value"),
        // Still resolves when empty: the storage is there, and a watch added
        // now must keep working once a value arrives.
        address("g_none.value"),
        address("g_vector[2]"),
        address("g_quat.w"),
    ];
    let missing = address("g_some.nothing").unwrap_err();
    let (array, some, none, vector, quat) = (
        base(&s, "g_array"),
        base(&s, "g_some"),
        base(&s, "g_none"),
        base(&s, "g_vector"),
        base(&s, "g_quat"),
    );
    assert_eq!(
        results.map(Result::unwrap),
        [array + 2, some, none, vector + 8, quat + 12]
    );
    assert!(missing.contains("no member"), "{missing}");

    // Without rules, the same names stay errors.
    let expr = symbols::parse("g_some.value").unwrap();
    assert!(s.db.evaluate(&expr, &mut s.memory).is_err());
}

#[test]
fn tuple_elements_are_reached_by_index() {
    let mut s = setup();
    assert_eq!(s.child_names("g_tuple"), ["[0]", "[1]", "[2]"]);
    let mut read = |text: &str| {
        let expr = symbols::parse(text).unwrap();
        match s.db.evaluate_virtual(&expr, &mut s.memory, Some(&s.rules)) {
            Ok(symbols::Evaluated::Place(place)) => {
                s.db.render_place(&place, &mut s.memory, &Default::default())
                    .unwrap()
            }
            other => panic!("{text}: {other:?}"),
        }
    };
    assert_eq!(read("g_tuple[0]"), "5");
    assert_eq!(read("g_tuple[2]"), "2.5");
}

#[test]
fn variant_shows_the_active_alternative() {
    let mut s = setup();
    assert_eq!(s.summary("g_variant"), "index 1: 1.25");
}

#[test]
fn span_and_string_view_follow_their_pointers() {
    let mut s = setup();
    assert_eq!(s.summary("g_span"), "[11, 22, 33]");
    assert_eq!(s.summary("g_view"), "\"hello\"");
}

#[test]
fn etl_vector_uses_its_live_size() {
    let mut s = setup();
    assert_eq!(s.summary("g_etl"), "[5, 6]");
}

#[test]
fn types_without_a_rule_get_the_default_display() {
    let mut s = setup();
    assert_eq!(s.apply("g_plain"), None);
    assert_eq!(s.apply("g_span_data"), None);
}

#[test]
fn a_failing_rule_falls_back_instead_of_erroring() {
    let mut s = setup();
    s.rules
        .load_file(Path::new(&write_rule(
            "broken",
            r#"visualizer { match = "^Plain$", summary = function(v) return v:field("nope"):str() end }"#,
        )))
        .unwrap();
    assert_eq!(s.apply("g_plain"), None);
}

#[test]
fn project_rules_override_builtin_ones() {
    let mut s = setup();
    s.rules
        .load_file(Path::new(&write_rule(
            "override",
            r#"
            visualizer {
              match = "^std::array<",
              summary = function(v) return "mine:" .. #v:field("_M_elems"):elements() end,
            }
            visualizer {
              match = "^Plain$",
              summary = function(v) return "a is " .. v:field("a"):str() end,
            }
            "#,
        )))
        .unwrap();
    assert_eq!(s.summary("g_array"), "mine:3");
    assert_eq!(s.summary("g_plain"), "a is 3");
}

fn write_rule(name: &str, text: &str) -> String {
    let path = std::env::temp_dir().join(format!("haltd-{}-{name}.lua", std::process::id()));
    std::fs::write(&path, text).unwrap();
    path.display().to_string()
}
