//! Rule coverage on a full firmware image, run on demand:
//! `JLINKD_TEST_ELF=fw.elf cargo test --release -p visualizer --test real_elf -- --ignored --nocapture`

use std::collections::BTreeMap;
use std::path::Path;

use symbols::{LoadOptions, MemoryReader, Place, SymbolDb, TypeKind};
use visualizer::Visualizers;

const MAX_DEPTH: usize = 5;

/// All-zero memory: reachable values are trivial, but every type is exercised.
struct ZeroMemory;

impl MemoryReader for ZeroMemory {
    fn read(&mut self, _addr: u32, buf: &mut [u8]) -> Result<(), String> {
        buf.fill(0);
        Ok(())
    }
}

#[derive(Default)]
struct Tally {
    applied: BTreeMap<String, (usize, String)>,
    unhandled: BTreeMap<String, usize>,
}

const RULE_TARGETS: [&str; 12] = [
    "units::unit_t<",
    "Eigen::Matrix<",
    "Eigen::Quaternion<",
    "std::array<",
    "std::optional<",
    "std::variant<",
    "std::span<",
    "std::basic_string_view<char",
    "etl::vector<",
    "etl::ivector<",
    "etl::array<",
    "etl::string",
];

fn family(name: &str) -> Option<&'static str> {
    RULE_TARGETS
        .iter()
        .find(|prefix| name.starts_with(**prefix))
        .copied()
}

fn visit(db: &SymbolDb, rules: &Visualizers, place: Place, depth: usize, tally: &mut Tally) {
    if depth > MAX_DEPTH {
        return;
    }
    if let Ok((resolved, info)) = db.strip(place.ty) {
        if matches!(info.kind, TypeKind::Pointer(_) | TypeKind::Reference(_)) {
            return;
        }
        if let Ok(Some(name)) = db.qualified_type_name(resolved) {
            if let Some(prefix) = family(&name) {
                match rules.apply(db, &mut ZeroMemory, place) {
                    Some(result) => {
                        let entry = tally
                            .applied
                            .entry(prefix.to_owned())
                            .or_insert((0, String::new()));
                        entry.0 += 1;
                        entry.1 = result.summary.unwrap_or_default();
                    }
                    None => *tally.unhandled.entry(name).or_default() += 1,
                }
            }
        }
    }
    let Ok(children) = db.children(&place, &mut ZeroMemory) else {
        return;
    };
    for (_, child) in children.into_iter().take(8) {
        visit(db, rules, child, depth + 1, tally);
    }
}

#[test]
#[ignore = "needs JLINKD_TEST_ELF"]
fn rules_cover_the_firmware() {
    let elf = std::env::var("JLINKD_TEST_ELF").expect("JLINKD_TEST_ELF");
    let db = SymbolDb::load(Path::new(&elf), &LoadOptions::default()).unwrap();
    let rules = Visualizers::with_builtin().unwrap();
    let mut tally = Tally::default();
    for variable in db.all_globals() {
        if let Ok(Some(ty)) = db.variable_type(&variable) {
            visit(&db, &rules, Place::new(variable.addr, ty), 0, &mut tally);
        }
    }
    for (family, (count, sample)) in &tally.applied {
        println!("{count:6} {family:30} e.g. {sample}");
    }
    println!("unhandled: {}", tally.unhandled.len());
    for (name, count) in tally.unhandled.iter().take(20) {
        println!("{count:6} {}", name.chars().take(100).collect::<String>());
    }
}

/// Whether expressions resolve on a full image, run on demand with
/// `JLINKD_TEST_ELF=fw.elf JLINKD_TEST_EXPR="a.b,c[1]"`. All-zero memory also
/// means every `std::optional` on the way is empty.
#[test]
#[ignore = "needs JLINKD_TEST_ELF and JLINKD_TEST_EXPR"]
fn expressions_resolve_on_an_image() {
    let elf = std::env::var("JLINKD_TEST_ELF").expect("JLINKD_TEST_ELF");
    let expressions = std::env::var("JLINKD_TEST_EXPR").expect("JLINKD_TEST_EXPR");
    let db = SymbolDb::load(Path::new(&elf), &LoadOptions::default()).unwrap();
    let rules = Visualizers::with_builtin().unwrap();
    for text in expressions.split(',') {
        let expr = symbols::parse(text).unwrap();
        let result = db.evaluate_virtual(&expr, &mut ZeroMemory, Some(&rules));
        match result {
            Ok(symbols::Evaluated::Place(place)) => {
                println!("{text}: {:#010x} {}", place.addr, db.type_name(place.ty));
            }
            other => panic!("{text}: {other:?}"),
        }
    }
}
