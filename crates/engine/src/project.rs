//! `.jlinkd/project.yaml` (DESIGN §7.2), the parts the debug engine uses.

use std::path::Path;

use serde::Deserialize;

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct ProjectConfig {
    pub target: TargetConfig,
    pub debug: DebugConfig,
    /// Rule files, relative to the workspace.
    pub visualizers: Vec<String>,
    pub sampling: SamplingOptions,
    pub rtt: RttOptions,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct RttOptions {
    /// Decoder configuration, relative to the workspace.
    pub config: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct SamplingOptions {
    pub hss: HssOptions,
    pub watch_rate_hz: u32,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct HssOptions {
    pub period_us: u32,
}

impl Default for HssOptions {
    fn default() -> Self {
        Self { period_us: 1000 }
    }
}

impl Default for SamplingOptions {
    fn default() -> Self {
        Self {
            hss: HssOptions::default(),
            watch_rate_hz: 5,
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct TargetConfig {
    /// Profile under `devices/`; inferred from the device name when absent.
    pub family: Option<String>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FlashBreakpoints {
    #[default]
    Auto,
    Never,
    Always,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct DebugConfig {
    pub mask_interrupts_on_step: bool,
    pub vector_catch: bool,
    pub freeze_on_halt: Vec<String>,
    pub flash_breakpoints: FlashBreakpoints,
}

impl Default for DebugConfig {
    fn default() -> Self {
        Self {
            mask_interrupts_on_step: true,
            vector_catch: true,
            freeze_on_halt: Vec::new(),
            flash_breakpoints: FlashBreakpoints::Auto,
        }
    }
}

impl ProjectConfig {
    pub fn load(path: &Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(path)
            .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
        serde_yaml::from_str(&text).map_err(|error| format!("invalid {}: {error}", path.display()))
    }
}

/// Built-in profile name for a J-Link device string.
pub fn family_for_device(device: &str) -> Option<&'static str> {
    let device = device.to_ascii_uppercase();
    if device.starts_with("STM32H7") {
        Some("stm32h7")
    } else if device.starts_with("STM32F1") {
        Some("stm32f1")
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_sections_take_defaults() {
        let config: ProjectConfig = serde_yaml::from_str("target: { family: stm32h7 }").unwrap();
        assert_eq!(config.target.family.as_deref(), Some("stm32h7"));
        assert!(config.debug.mask_interrupts_on_step);
        assert_eq!(config.debug.flash_breakpoints, FlashBreakpoints::Auto);
    }

    #[test]
    fn debug_options_parse() {
        let config: ProjectConfig = serde_yaml::from_str(
            "debug: { maskInterruptsOnStep: false, flashBreakpoints: never, freezeOnHalt: [iwdg1] }",
        )
        .unwrap();
        assert!(!config.debug.mask_interrupts_on_step);
        assert_eq!(config.debug.flash_breakpoints, FlashBreakpoints::Never);
        assert_eq!(config.debug.freeze_on_halt, ["iwdg1"]);
    }

    #[test]
    fn device_names_map_to_families() {
        assert_eq!(family_for_device("STM32H723VG"), Some("stm32h7"));
        assert_eq!(family_for_device("stm32f103c8"), Some("stm32f1"));
        assert_eq!(family_for_device("nRF52840"), None);
    }
}
