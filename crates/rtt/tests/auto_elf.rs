#![cfg(has_arm_gcc)]

use std::path::Path;

use rtt::decode::{auto_decoder, Sink};
use symbols::{LoadOptions, SymbolDb};

const FIXTURE: &str = env!("FIXTURE_ELF");

#[derive(Default)]
struct Collect {
    samples: Vec<(String, i64, f64, bool)>,
    text: Vec<u8>,
}

impl Sink for Collect {
    fn sample(&mut self, name: &str, _group: Option<&str>, plot: bool, t: i64, value: f64) {
        self.samples.push((name.to_owned(), t, value, plot));
    }
    fn text(&mut self, data: &[u8]) {
        self.text.extend_from_slice(data);
    }
}

fn load_db() -> SymbolDb {
    SymbolDb::load(
        Path::new(FIXTURE),
        &LoadOptions {
            no_cache: true,
            ..LoadOptions::default()
        },
    )
    .unwrap()
}

#[test]
fn auto_decoder_picks_struct_and_typedef_from_elf() {
    let db = load_db();

    let (mut dec, label, problem) = auto_decoder(&db, "TelemetryNoSync", 1);
    assert_eq!(label, "auto-struct");
    assert!(problem.as_deref().unwrap().contains("NO_BLOCK_SKIP"));
    let mut sink = Collect::default();
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&(-12i16).to_le_bytes());
    bytes.extend_from_slice(&[0, 0]);
    bytes.extend_from_slice(&3.5f32.to_le_bytes());
    dec.feed(&bytes, 100, &mut sink);
    assert_eq!(
        sink.samples,
        [
            ("TelemetryNoSync.speed".to_owned(), 100, -12.0, true),
            ("TelemetryNoSync.current".to_owned(), 100, 3.5, true),
        ]
    );

    let (mut dec_prefixed, label_prefixed, _) = auto_decoder(&db, "struct:TypedefFrame", 2);
    assert_eq!(label_prefixed, "auto-struct");
    let mut sink2 = Collect::default();
    let mut tf_bytes = Vec::new();
    tf_bytes.extend_from_slice(&42u32.to_le_bytes());
    tf_bytes.extend_from_slice(&12.25f32.to_le_bytes());
    dec_prefixed.feed(&tf_bytes, 200, &mut sink2);
    assert_eq!(
        sink2.samples,
        [
            ("struct:TypedefFrame.count".to_owned(), 200, 42.0, true),
            ("struct:TypedefFrame.voltage".to_owned(), 200, 12.25, true),
        ]
    );
}

#[test]
fn auto_decoder_handles_sync_field_and_non_structs() {
    let db = load_db();

    let (_, label_missing, problem) = auto_decoder(&db, "TelemetryWithSync", 1);
    assert_eq!(label_missing, "text");
    assert!(problem
        .as_deref()
        .unwrap()
        .contains("TelemetryWithSync@0x<value>"));

    let (mut dec_sync, label_sync, problem_sync) = auto_decoder(&db, "TelemetryWithSync@0xA55A", 1);
    assert_eq!(label_sync, "auto-struct");
    assert_eq!(problem_sync, None);
    let mut sink = Collect::default();
    let mut stream = Vec::new();
    for seq in [1u16, 2u16] {
        stream.extend_from_slice(&0xA55Au16.to_le_bytes());
        stream.extend_from_slice(&seq.to_le_bytes());
        stream.extend_from_slice(&(seq as f32 * 1.5).to_le_bytes());
    }
    dec_sync.feed(&stream, 300, &mut sink);
    assert_eq!(
        sink.samples,
        [
            ("TelemetryWithSync@0xA55A.iq".to_owned(), 300, 1.5, true),
            ("TelemetryWithSync@0xA55A.iq".to_owned(), 300, 3.0, true),
        ]
    );

    // Non-struct types (like enums or plain channel names) fall back to key=value text.
    let (mut dec_enum, label_enum, _) = auto_decoder(&db, "Mode", 0);
    assert_eq!(label_enum, "auto-text");
    let mut enum_sink = Collect::default();
    dec_enum.feed(b"tick=5\n", 400, &mut enum_sink);
    assert_eq!(
        enum_sink.samples,
        [("Mode.tick".to_owned(), 400, 5.0, true)]
    );

    let (_, label_term, problem_term) = auto_decoder(&db, "Terminal", 0);
    assert_eq!(label_term, "auto-text");
    assert_eq!(problem_term, None);
}
