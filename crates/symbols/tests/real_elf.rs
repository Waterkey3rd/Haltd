//! Robustness check against a full firmware image, run on demand:
//!
//! ```text
//! arm-none-eabi-nm -C --defined-only fw.elf | awk '$2 ~ /[bBdD]/ && NF==3 {print $3}' > names
//! JLINKD_TEST_ELF=fw.elf JLINKD_TEST_NAMES=names \\
//!     cargo test --release -p symbols --test real_elf -- --ignored --nocapture
//! ```

use std::collections::BTreeMap;
use std::path::Path;

use symbols::{FormatOptions, LoadOptions, MemoryReader, SymbolDb};

/// Lets every object be read, so failures can only come from symbol handling.
struct ZeroMemory;

impl MemoryReader for ZeroMemory {
    fn read(&mut self, _addr: u32, buf: &mut [u8]) -> Result<(), String> {
        buf.fill(0);
        Ok(())
    }
}

#[test]
#[ignore = "needs JLINKD_TEST_ELF and JLINKD_TEST_NAMES"]
fn linker_data_symbols_resolve_and_render() {
    let elf = std::env::var("JLINKD_TEST_ELF").expect("JLINKD_TEST_ELF");
    let names = std::env::var("JLINKD_TEST_NAMES").expect("JLINKD_TEST_NAMES");
    let names = std::fs::read_to_string(names).expect("names file");
    let db = SymbolDb::load(Path::new(&elf), &LoadOptions::default()).unwrap();

    let mut rendered = 0usize;
    let mut failures = BTreeMap::<String, Vec<String>>::new();
    for name in names.lines().filter(|line| !line.is_empty()) {
        match db.read_expression(name, &mut ZeroMemory, &FormatOptions::default()) {
            Ok(_) => rendered += 1,
            Err(error) => failures
                .entry(error.to_string().chars().take(48).collect())
                .or_default()
                .push(name.to_owned()),
        }
    }
    println!("rendered {rendered}");
    for (error, names) in &failures {
        println!("{:5} x {error} | e.g. {}", names.len(), names[0]);
    }
}
