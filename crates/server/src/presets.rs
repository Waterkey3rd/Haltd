//! Persistent workspace Timeline presets (`.haltd/timeline-presets.json`).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

const PRESETS_VERSION: u32 = 1;
const PRESETS_FILE: &str = "timeline-presets.json";

const fn default_version() -> u32 {
    PRESETS_VERSION
}

const fn default_true() -> bool {
    true
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct PresetSignal {
    pub kind: String,
    pub expr: String,
    pub mode: String,
    pub group: Option<String>,
    pub color: Option<String>,
    #[serde(default)]
    pub hidden: bool,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct PresetTrack {
    #[serde(default = "default_true")]
    pub auto_y: bool,
    pub y_min: Option<f64>,
    pub y_max: Option<f64>,
    pub height_px: Option<u32>,
    #[serde(default)]
    pub collapsed: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct TimelinePreset {
    pub name: String,
    #[serde(default)]
    pub updated_at: String,
    pub rate_hz: Option<u32>,
    pub division: Option<f64>,
    #[serde(default)]
    pub split_lanes: bool,
    #[serde(default = "default_true")]
    pub show_bands: bool,
    #[serde(default)]
    pub signals: Vec<PresetSignal>,
    #[serde(default)]
    pub tracks: BTreeMap<String, PresetTrack>,
}

impl Default for TimelinePreset {
    fn default() -> Self {
        Self {
            name: String::new(),
            updated_at: String::new(),
            rate_hz: None,
            division: None,
            split_lanes: false,
            show_bands: true,
            signals: Vec::new(),
            tracks: BTreeMap::new(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct TimelinePresetFile {
    #[serde(default = "default_version")]
    pub version: u32,
    pub active_preset: Option<String>,
    #[serde(default)]
    pub presets: Vec<TimelinePreset>,
}

impl Default for TimelinePresetFile {
    fn default() -> Self {
        Self {
            version: PRESETS_VERSION,
            active_preset: None,
            presets: Vec::new(),
        }
    }
}

pub(crate) fn presets_path(workspace: Option<&Path>) -> Result<PathBuf, String> {
    if let Some(ws) = workspace {
        return Ok(ws.join(".haltd").join(PRESETS_FILE));
    }
    symbols::paths::cache_root()
        .map(|root| root.join(PRESETS_FILE))
        .ok_or_else(|| "no workspace or cache directory available for presets".to_owned())
}

pub(crate) fn load(workspace: Option<&Path>) -> Result<TimelinePresetFile, String> {
    let path = presets_path(workspace)?;
    if !path.exists() {
        return Ok(TimelinePresetFile::default());
    }
    let raw = std::fs::read_to_string(&path)
        .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
    if raw.trim().is_empty() {
        return Ok(TimelinePresetFile::default());
    }
    serde_json::from_str(&raw)
        .map_err(|error| format!("invalid presets file {}: {error}", path.display()))
}

pub(crate) fn store(workspace: Option<&Path>, data: &TimelinePresetFile) -> Result<(), String> {
    let path = presets_path(workspace)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("cannot create {}: {error}", parent.display()))?;
    }
    let json = serde_json::to_string_pretty(data).map_err(|error| error.to_string())?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, format!("{json}\n"))
        .map_err(|error| format!("cannot write {}: {error}", tmp.display()))?;
    if let Err(rename_err) = std::fs::rename(&tmp, &path) {
        std::fs::write(&path, format!("{json}\n"))
            .map_err(|error| format!("cannot write {}: {error} ({rename_err})", path.display()))?;
        let _ = std::fs::remove_file(&tmp);
    }
    Ok(())
}

pub(crate) fn save_preset(
    workspace: Option<&Path>,
    mut preset: TimelinePreset,
    overwrite_name: Option<&str>,
) -> Result<TimelinePresetFile, String> {
    let name = preset.name.trim().to_owned();
    if name.is_empty() {
        return Err("preset name cannot be empty".to_owned());
    }
    preset.name = name.clone();
    let mut file = load(workspace)?;
    let target_name = overwrite_name.unwrap_or(&name);
    if let Some(pos) = file.presets.iter().position(|p| p.name == target_name) {
        if target_name != name {
            file.presets.retain(|p| p.name != name);
        }
        if let Some(slot) = file.presets.iter_mut().find(|p| p.name == target_name) {
            *slot = preset;
        } else if pos < file.presets.len() {
            file.presets[pos] = preset;
        } else {
            file.presets.push(preset);
        }
    } else if let Some(existing) = file.presets.iter_mut().find(|p| p.name == name) {
        *existing = preset;
    } else {
        file.presets.push(preset);
    }
    file.active_preset = Some(name);
    store(workspace, &file)?;
    Ok(file)
}

pub(crate) fn delete_preset(
    workspace: Option<&Path>,
    name: &str,
) -> Result<TimelinePresetFile, String> {
    let mut file = load(workspace)?;
    let before = file.presets.len();
    file.presets.retain(|p| p.name != name);
    if file.presets.len() == before {
        return Err(format!("no preset named {name:?}"));
    }
    if file.active_preset.as_deref() == Some(name) {
        file.active_preset = None;
    }
    store(workspace, &file)?;
    Ok(file)
}

pub(crate) fn rename_preset(
    workspace: Option<&Path>,
    from: &str,
    to: &str,
) -> Result<TimelinePresetFile, String> {
    let new_name = to.trim();
    if new_name.is_empty() {
        return Err("new preset name cannot be empty".to_owned());
    }
    let mut file = load(workspace)?;
    if from != new_name && file.presets.iter().any(|p| p.name == new_name) {
        return Err(format!("a preset named {new_name:?} already exists"));
    }
    let preset = file
        .presets
        .iter_mut()
        .find(|p| p.name == from)
        .ok_or_else(|| format!("no preset named {from:?}"))?;
    preset.name = new_name.to_owned();
    if file.active_preset.as_deref() == Some(from) {
        file.active_preset = Some(new_name.to_owned());
    }
    store(workspace, &file)?;
    Ok(file)
}

pub(crate) fn set_active(
    workspace: Option<&Path>,
    name: Option<&str>,
) -> Result<TimelinePresetFile, String> {
    let mut file = load(workspace)?;
    file.active_preset = name.map(str::to_owned);
    store(workspace, &file)?;
    Ok(file)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preset_crud_persists_in_workspace_haltd_dir() {
        let dir = std::env::temp_dir().join(format!("haltd-presets-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let p1 = TimelinePreset {
            name: "Recomp".to_owned(),
            rate_hz: Some(1000),
            division: Some(0.2),
            signals: vec![PresetSignal {
                kind: "expr".to_owned(),
                expr: "g_val".to_owned(),
                mode: "hss".to_owned(),
                group: Some("Track 1".to_owned()),
                color: Some("#4dabf7".to_owned()),
                hidden: false,
            }],
            ..TimelinePreset::default()
        };
        let saved = save_preset(Some(&dir), p1, None).unwrap();
        assert_eq!(saved.presets.len(), 1);
        assert_eq!(saved.active_preset.as_deref(), Some("Recomp"));

        let renamed = rename_preset(Some(&dir), "Recomp", "CommFreq").unwrap();
        assert_eq!(renamed.presets[0].name, "CommFreq");
        assert_eq!(renamed.active_preset.as_deref(), Some("CommFreq"));

        let deleted = delete_preset(Some(&dir), "CommFreq").unwrap();
        assert!(deleted.presets.is_empty());
        assert_eq!(deleted.active_preset, None);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
