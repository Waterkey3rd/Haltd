//! `.haltd/rtt.yaml` (DESIGN §7.3).

use std::collections::BTreeMap;
use std::path::Path;

use serde::Deserialize;

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RttConfig {
    /// `auto`, or an address.
    #[serde(default)]
    pub control_block: Option<serde_yaml::Value>,
    #[serde(default)]
    pub channels: BTreeMap<u32, ChannelConfig>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct ChannelConfig {
    pub name: String,
    #[serde(flatten)]
    pub decode: Decode,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum Decode {
    Text {
        #[serde(default)]
        ansi: bool,
    },
    TextRegex {
        pattern: String,
        #[serde(default)]
        group: Option<String>,
    },
    Struct(StructConfig),
    Lua {
        script: String,
    },
    Systemview {
        file: String,
    },
}

#[derive(Clone, Debug, Deserialize)]
pub struct StructConfig {
    pub sync: u32,
    #[serde(rename = "struct")]
    pub type_name: String,
    #[serde(default)]
    pub timestamp: Option<Timestamp>,
    /// Fields to plot. Absent means every numeric field.
    #[serde(default)]
    pub timeline: Option<Vec<TimelineEntry>>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Timestamp {
    pub field: String,
    pub clock_hz: f64,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(untagged)]
pub enum TimelineEntry {
    Field(String),
    Grouped {
        expr: String,
        #[serde(default)]
        group: Option<String>,
    },
}

impl TimelineEntry {
    /// Selected field names, expanding `name[a..b]` (inclusive) ranges.
    pub fn expand(&self) -> (Vec<String>, Option<String>) {
        let (expr, group) = match self {
            TimelineEntry::Field(expr) => (expr.as_str(), None),
            TimelineEntry::Grouped { expr, group } => (expr.as_str(), group.clone()),
        };
        (expand_range(expr), group)
    }
}

fn expand_range(expr: &str) -> Vec<String> {
    let parsed = (|| {
        let (head, rest) = expr.split_once('[')?;
        let (range, tail) = rest.split_once(']')?;
        let (low, high) = range.split_once("..")?;
        Some((
            head,
            low.parse::<u32>().ok()?,
            high.parse::<u32>().ok()?,
            tail,
        ))
    })();
    match parsed {
        Some((head, low, high, tail)) => (low..=high)
            .map(|index| format!("{head}[{index}]{tail}"))
            .collect(),
        None => vec![expr.to_owned()],
    }
}

impl RttConfig {
    pub fn load(path: &Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(path)
            .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
        serde_yaml::from_str(&text).map_err(|error| format!("invalid {}: {error}", path.display()))
    }

    /// An explicit control block address, if one was given.
    pub fn control_block_address(&self) -> Option<u32> {
        self.control_block
            .as_ref()
            .and_then(serde_yaml::Value::as_u64)
            .map(|address| address as u32)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_documented_format_parses() {
        let config: RttConfig = serde_yaml::from_str(
            r#"
controlBlock: auto
channels:
  0: { name: log, type: text, ansi: true }
  1: { name: sysview, type: systemview, file: build/trace.SVDat }
  2:
    name: telemetry
    type: struct
    sync: 0xA55A
    struct: haltd::TelemetryFrame
    timestamp: { field: cyccnt, clockHz: 540000000 }
    timeline:
      - jointPos[0..5]
      - { expr: loopUs, group: timing }
  3: { name: motor, type: lua, script: .haltd/decode_motor.lua }
  4: { name: t, type: text-regex, pattern: 'a=(?P<a>\d+)' }
"#,
        )
        .unwrap();
        assert_eq!(config.channels.len(), 5);
        assert!(config.control_block_address().is_none());
        let Decode::Struct(frame) = &config.channels[&2].decode else {
            panic!("expected a struct channel");
        };
        assert_eq!(frame.sync, 0xA55A);
        assert_eq!(frame.timestamp.as_ref().unwrap().clock_hz, 540e6);
        let entries = frame.timeline.as_ref().unwrap();
        assert_eq!(entries[0].expand().0.len(), 6);
        assert_eq!(entries[1].expand().1.as_deref(), Some("timing"));
    }

    #[test]
    fn an_explicit_address_is_used() {
        let config: RttConfig = serde_yaml::from_str("controlBlock: 0x24000000").unwrap();
        assert_eq!(config.control_block_address(), Some(0x2400_0000));
    }

    #[test]
    fn ranges_expand_inclusively_and_keep_a_suffix() {
        assert_eq!(expand_range("a[1..2].b"), ["a[1].b", "a[2].b"]);
        assert_eq!(expand_range("plain"), ["plain"]);
        assert_eq!(expand_range("a[3]"), ["a[3]"]);
    }
}
