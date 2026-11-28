//! CMSIS-SVD peripheral descriptions (DESIGN F13).
//!
//! Handles what vendor files use: inherited size and access, `derivedFrom`,
//! `dim` arrays and clusters.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use roxmltree::{Document, Node};

#[cfg(not(windows))]
const SEGGER_DIR: &str = "/opt/SEGGER";
#[cfg(windows)]
const SEGGER_DIR: &str = r"C:\Program Files\SEGGER";
const OZONE_PERIPHERALS: &str = "Config/Peripherals";
const DEFAULT_REGISTER_BITS: u32 = 32;

/// Data registers that lose data or clear flags when read. Vendor files rarely
/// mark them, and reading one could swallow a byte the firmware is waiting for.
const FIFO_PERIPHERALS: [&str; 11] = [
    "USART", "UART", "LPUART", "SPI", "I2S", "I2C", "ADC", "SDIO", "SDMMC", "DCMI", "SAI",
];
const FIFO_REGISTERS: [&str; 9] = [
    "DR", "RDR", "RXDR", "RXDATA", "FIFO", "RXCRC", "CDR", "ADR", "BDR",
];

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Access {
    #[default]
    ReadWrite,
    ReadOnly,
    WriteOnly,
}

impl Access {
    fn parse(text: &str) -> Option<Self> {
        match text.trim() {
            "read-only" => Some(Self::ReadOnly),
            "write-only" | "writeOnce" => Some(Self::WriteOnly),
            "read-write" | "read-writeOnce" => Some(Self::ReadWrite),
            _ => None,
        }
    }

    pub fn can_write(self) -> bool {
        self != Self::ReadOnly
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Field {
    pub name: String,
    pub description: String,
    pub lsb: u32,
    pub width: u32,
    pub access: Access,
    /// Value names, as declared.
    pub values: Vec<(u64, String)>,
}

impl Field {
    pub fn mask(&self) -> u32 {
        let bits = if self.width >= 32 {
            u32::MAX
        } else {
            (1u32 << self.width) - 1
        };
        bits << self.lsb
    }

    pub fn extract(&self, register: u32) -> u32 {
        (register & self.mask()) >> self.lsb
    }

    /// `register` with this field replaced, or None if `value` does not fit.
    pub fn insert(&self, register: u32, value: u32) -> Option<u32> {
        let shifted = value.checked_shl(self.lsb)?;
        (shifted >> self.lsb == value && shifted & !self.mask() == 0)
            .then_some((register & !self.mask()) | shifted)
    }

    pub fn value_name(&self, value: u32) -> Option<&str> {
        self.values
            .iter()
            .find(|(known, _)| *known == u64::from(value))
            .map(|(_, name)| name.as_str())
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Register {
    pub name: String,
    pub description: String,
    pub address: u32,
    pub bits: u32,
    pub access: Access,
    /// Must only be read when the user explicitly asks.
    pub read_has_side_effects: bool,
    pub fields: Vec<Field>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Peripheral {
    pub name: String,
    pub description: String,
    pub base: u32,
    pub registers: Vec<Register>,
}

impl Peripheral {
    pub fn register(&self, name: &str) -> Option<&Register> {
        self.registers.iter().find(|register| register.name == name)
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Device {
    pub name: String,
    pub peripherals: Vec<Peripheral>,
    pub interrupts: HashMap<u32, String>,
}

impl Device {
    pub fn peripheral(&self, name: &str) -> Option<&Peripheral> {
        self.peripherals
            .iter()
            .find(|peripheral| peripheral.name == name)
    }
}

/// Size and access that registers inherit from their enclosing elements.
#[derive(Clone, Copy)]
struct Defaults {
    bits: u32,
    access: Access,
}

fn child<'a, 'input>(node: Node<'a, 'input>, name: &str) -> Option<Node<'a, 'input>> {
    node.children().find(|c| c.has_tag_name(name))
}

fn text<'a>(node: Node<'a, '_>, name: &str) -> Option<&'a str> {
    child(node, name).and_then(|c| c.text()).map(str::trim)
}

fn number(value: &str) -> Option<u64> {
    let value = value.trim();
    if let Some(hex) = value
        .strip_prefix("0x")
        .or_else(|| value.strip_prefix("0X"))
    {
        return u64::from_str_radix(hex, 16).ok();
    }
    if let Some(bits) = value.strip_prefix('#') {
        return u64::from_str_radix(bits, 2).ok();
    }
    if let Some(bits) = value
        .strip_prefix("0b")
        .or_else(|| value.strip_prefix("0B"))
    {
        return u64::from_str_radix(bits, 2).ok();
    }
    let (digits, scale) = match value.as_bytes().last() {
        Some(b'k' | b'K') => (&value[..value.len() - 1], 1 << 10),
        Some(b'm' | b'M') => (&value[..value.len() - 1], 1 << 20),
        Some(b'g' | b'G') => (&value[..value.len() - 1], 1 << 30),
        _ => (value, 1),
    };
    digits.parse::<u64>().ok().map(|n| n * scale)
}

fn number_of(node: Node, name: &str) -> Option<u64> {
    text(node, name).and_then(number)
}

fn clean(value: Option<&str>) -> String {
    value
        .unwrap_or("")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn inherit(node: Node, defaults: Defaults) -> Defaults {
    Defaults {
        bits: number_of(node, "size").map_or(defaults.bits, |bits| bits as u32),
        access: text(node, "access")
            .and_then(Access::parse)
            .unwrap_or(defaults.access),
    }
}

/// Instances of a `dim` element: (name suffix, address offset).
fn dim_instances(node: Node) -> Option<Vec<(String, u64)>> {
    let count = number_of(node, "dim")?;
    let step = number_of(node, "dimIncrement").unwrap_or(0);
    let labels: Vec<String> = match text(node, "dimIndex") {
        Some(list) if list.contains(',') => list.split(',').map(|s| s.trim().to_owned()).collect(),
        Some(range) if range.contains('-') => {
            let (start, end) = range.split_once('-')?;
            match (start.trim().parse::<u64>(), end.trim().parse::<u64>()) {
                (Ok(start), Ok(end)) => (start..=end).map(|i| i.to_string()).collect(),
                _ => {
                    let (first, last) = (start.trim().as_bytes()[0], end.trim().as_bytes()[0]);
                    (first..=last).map(|c| char::from(c).to_string()).collect()
                }
            }
        }
        _ => (0..count).map(|i| i.to_string()).collect(),
    };
    Some(
        labels
            .into_iter()
            .take(count as usize)
            .enumerate()
            .map(|(index, label)| (label, index as u64 * step))
            .collect(),
    )
}

fn instance_name(pattern: &str, label: &str) -> String {
    if pattern.contains("[%s]") {
        pattern.replace("[%s]", label)
    } else {
        pattern.replace("%s", label)
    }
}

fn enumerated_values(node: Node) -> Vec<(u64, String)> {
    let mut values = Vec::new();
    if let Some(list) = child(node, "enumeratedValues") {
        for value in list
            .children()
            .filter(|c| c.has_tag_name("enumeratedValue"))
        {
            if let (Some(name), Some(number)) = (text(value, "name"), number_of(value, "value")) {
                values.push((number, name.to_owned()));
            }
        }
    }
    values
}

fn bit_range(node: Node) -> Option<(u32, u32)> {
    if let (Some(offset), Some(width)) = (number_of(node, "bitOffset"), number_of(node, "bitWidth"))
    {
        return Some((offset as u32, width as u32));
    }
    if let (Some(lsb), Some(msb)) = (number_of(node, "lsb"), number_of(node, "msb")) {
        return Some((lsb as u32, (msb.checked_sub(lsb)? + 1) as u32));
    }
    let range = text(node, "bitRange")?;
    let (msb, lsb) = range.trim_matches(['[', ']']).split_once(':')?;
    let (msb, lsb): (u32, u32) = (msb.trim().parse().ok()?, lsb.trim().parse().ok()?);
    Some((lsb, msb.checked_sub(lsb)? + 1))
}

fn parse_field(node: Node, access: Access, out: &mut Vec<Field>) {
    let (Some(name), Some((lsb, width))) = (text(node, "name"), bit_range(node)) else {
        return;
    };
    let access = text(node, "access")
        .and_then(Access::parse)
        .unwrap_or(access);
    let description = clean(text(node, "description"));
    let values = enumerated_values(node);
    let instances = dim_instances(node);
    match instances {
        Some(instances) => {
            for (label, step) in instances {
                out.push(Field {
                    name: instance_name(name, &label),
                    description: description.clone(),
                    lsb: lsb + step as u32,
                    width,
                    access,
                    values: values.clone(),
                });
            }
        }
        None => out.push(Field {
            name: name.to_owned(),
            description,
            lsb,
            width,
            access,
            values,
        }),
    }
}

fn parse_fields(node: Node, access: Access) -> Vec<Field> {
    let mut fields = Vec::new();
    if let Some(list) = child(node, "fields") {
        for field in list.children().filter(|c| c.has_tag_name("field")) {
            parse_field(field, access, &mut fields);
        }
    }
    fields.sort_by_key(|field| std::cmp::Reverse(field.lsb));
    fields
}

fn side_effects(node: Node, peripheral: &str, register: &str) -> bool {
    if text(node, "readAction").is_some() {
        return true;
    }
    FIFO_PERIPHERALS.iter().any(|p| peripheral.starts_with(p)) && FIFO_REGISTERS.contains(&register)
}

/// Registers of a peripheral or cluster, by name for `derivedFrom`.
struct RegisterParser<'a> {
    peripheral: &'a str,
    seen: HashMap<String, Register>,
}

impl RegisterParser<'_> {
    fn register(
        &mut self,
        node: Node,
        base: u32,
        defaults: Defaults,
        out: &mut Vec<Register>,
    ) -> Option<()> {
        let template = node
            .attribute("derivedFrom")
            .and_then(|name| self.seen.get(name.rsplit('.').next()?).cloned());
        let name = text(node, "name")?;
        let offset = number_of(node, "addressOffset")?;
        let settings = inherit(node, defaults);
        let fields = parse_fields(node, settings.access);
        let description = clean(text(node, "description"));
        let has_own = |tag: &str| child(node, tag).is_some();
        let instances = dim_instances(node).unwrap_or_else(|| vec![(String::new(), 0)]);
        for (label, step) in instances {
            let full_name = if label.is_empty() {
                name.to_owned()
            } else {
                instance_name(name, &label)
            };
            let mut register = template.clone().unwrap_or_default();
            register.name.clone_from(&full_name);
            register.address = base.wrapping_add((offset + step) as u32);
            if !has_own("description") && template.is_some() {
                // Keep the description it inherited.
            } else {
                register.description.clone_from(&description);
            }
            if template.is_none() || has_own("size") {
                register.bits = settings.bits;
            }
            if template.is_none() || has_own("access") {
                register.access = settings.access;
            }
            if template.is_none() || has_own("fields") {
                register.fields.clone_from(&fields);
            }
            register.read_has_side_effects = side_effects(node, self.peripheral, &full_name)
                || template.as_ref().is_some_and(|t| t.read_has_side_effects);
            self.seen.insert(name.to_owned(), register.clone());
            out.push(register);
        }
        Some(())
    }

    fn cluster(
        &mut self,
        node: Node,
        base: u32,
        prefix: &str,
        defaults: Defaults,
        out: &mut Vec<Register>,
    ) -> Option<()> {
        let name = text(node, "name")?;
        let offset = number_of(node, "addressOffset")?;
        let settings = inherit(node, defaults);
        let instances = dim_instances(node).unwrap_or_else(|| vec![(String::new(), 0)]);
        for (label, step) in instances {
            let full = if label.is_empty() {
                name.to_owned()
            } else {
                instance_name(name, &label)
            };
            let address = base.wrapping_add((offset + step) as u32);
            let path = format!("{prefix}{full}.");
            let mut inner = Vec::new();
            self.block(node, address, &path, settings, &mut inner);
            out.extend(inner);
        }
        Some(())
    }

    fn block(
        &mut self,
        node: Node,
        base: u32,
        prefix: &str,
        defaults: Defaults,
        out: &mut Vec<Register>,
    ) {
        for item in node.children().filter(Node::is_element) {
            let before = out.len();
            if item.has_tag_name("register") {
                self.register(item, base, defaults, out);
            } else if item.has_tag_name("cluster") {
                self.cluster(item, base, prefix, defaults, out);
                continue;
            }
            for register in &mut out[before..] {
                register.name = format!("{prefix}{}", register.name);
            }
        }
    }
}

fn parse_peripheral(
    node: Node,
    device: Defaults,
    by_name: &HashMap<String, Peripheral>,
) -> Vec<Peripheral> {
    let Some(name) = text(node, "name") else {
        return Vec::new();
    };
    let Some(base) = number_of(node, "baseAddress") else {
        return Vec::new();
    };
    let settings = inherit(node, device);
    let base = base as u32;
    let template = node
        .attribute("derivedFrom")
        .and_then(|from| by_name.get(from));
    let mut registers = Vec::new();
    let own_registers = child(node, "registers");
    if let Some(block) = own_registers {
        let mut parser = RegisterParser {
            peripheral: name,
            seen: HashMap::new(),
        };
        parser.block(block, base, "", settings, &mut registers);
        registers.sort_by_key(|register| register.address);
    } else if let Some(template) = template {
        registers = template
            .registers
            .iter()
            .cloned()
            .map(|mut register| {
                register.address = register
                    .address
                    .wrapping_sub(template.base)
                    .wrapping_add(base);
                register
            })
            .collect();
    }
    let description = match text(node, "description") {
        Some(own) => clean(Some(own)),
        None => template.map_or_else(String::new, |t| t.description.clone()),
    };
    let instances = dim_instances(node).unwrap_or_else(|| vec![(String::new(), 0)]);
    instances
        .into_iter()
        .map(|(label, step)| {
            let step = step as u32;
            Peripheral {
                name: if label.is_empty() {
                    name.to_owned()
                } else {
                    instance_name(name, &label)
                },
                description: description.clone(),
                base: base.wrapping_add(step),
                registers: registers
                    .iter()
                    .cloned()
                    .map(|mut register| {
                        register.address = register.address.wrapping_add(step);
                        register
                    })
                    .collect(),
            }
        })
        .collect()
}

pub fn parse(xml: &str) -> Result<Device, String> {
    let document = Document::parse_with_options(
        xml,
        roxmltree::ParsingOptions {
            allow_dtd: true,
            ..roxmltree::ParsingOptions::default()
        },
    )
    .map_err(|error| format!("invalid SVD: {error}"))?;
    let root = document.root_element();
    let list = child(root, "peripherals").ok_or("the SVD file lists no peripherals")?;
    let defaults = inherit(
        root,
        Defaults {
            bits: DEFAULT_REGISTER_BITS,
            access: Access::ReadWrite,
        },
    );
    let nodes: Vec<Node> = list
        .children()
        .filter(|c| c.has_tag_name("peripheral"))
        .collect();
    // A peripheral may derive from one declared after it.
    let (plain, derived): (Vec<Node>, Vec<Node>) = nodes
        .iter()
        .copied()
        .partition(|node| node.attribute("derivedFrom").is_none());
    let mut by_name: HashMap<String, Peripheral> = HashMap::new();
    let mut peripherals = Vec::new();
    let mut interrupts: HashMap<u32, String> = HashMap::new();
    for node in plain.iter().chain(derived.iter()) {
        for interrupt in node.children().filter(|c| c.has_tag_name("interrupt")) {
            if let (Some(irq_name), Some(value)) =
                (text(interrupt, "name"), number_of(interrupt, "value"))
            {
                if let Ok(irq_num) = u32::try_from(value) {
                    interrupts
                        .entry(irq_num)
                        .or_insert_with(|| irq_name.to_owned());
                }
            }
        }
        for peripheral in parse_peripheral(*node, defaults, &by_name) {
            by_name.insert(peripheral.name.clone(), peripheral.clone());
            peripherals.push(peripheral);
        }
    }
    peripherals.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(Device {
        name: text(root, "name").unwrap_or("").to_owned(),
        peripherals,
        interrupts,
    })
}

pub fn load(path: &Path) -> Result<Device, String> {
    let xml = std::fs::read_to_string(path)
        .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
    parse(&xml).map_err(|error| format!("{}: {error}", path.display()))
}

/// Strip J-Link core/package annotations such as `_M7`, ` (M4)`, etc.
pub fn normalize_device_name(device: &str) -> String {
    let trimmed = device.trim();
    let base = trimmed
        .split([' ', '(', '/', '_'])
        .next()
        .unwrap_or(trimmed);
    base.to_ascii_uppercase()
}

fn core_tag(text: &str) -> Option<&'static str> {
    let upper = text.to_ascii_uppercase();
    if upper.ends_with("_CM7") || upper.ends_with("_M7") {
        Some("M7")
    } else if upper.ends_with("_CM4") || upper.ends_with("_M4") {
        Some("M4")
    } else if upper.ends_with("_CM33") || upper.ends_with("_M33") {
        Some("M33")
    } else if upper.ends_with("_CM0P") || upper.ends_with("_M0P") || upper.ends_with("_CM0+") {
        Some("M0P")
    } else {
        None
    }
}

fn strip_core_suffix(upper: &str) -> &str {
    for suffix in [
        "_CM33", "_M33", "_CM7", "_M7", "_CM4", "_M4", "_CM0P", "_M0P", "_CM0+",
    ] {
        if let Some(stripped) = upper.strip_suffix(suffix) {
            return stripped;
        }
    }
    upper
}

/// Score how well an SVD filename or stem matches `device` (higher is better).
/// Supports trailing `xx` as well as wildcard `x` placeholders like `STM32H7x3` or `STM32L4x6`.
pub fn match_svd_score(device: &str, filename_or_stem: &str) -> Option<u32> {
    let dev = normalize_device_name(device);
    let dev_core = core_tag(device);
    if dev.is_empty() {
        return None;
    }
    let stem = filename_or_stem
        .strip_suffix(".svd")
        .or_else(|| filename_or_stem.strip_suffix(".SVD"))
        .unwrap_or(filename_or_stem)
        .to_ascii_uppercase();
    let stem_core = core_tag(&stem);
    if let (Some(dc), Some(sc)) = (dev_core, stem_core) {
        if dc != sc {
            return None;
        }
    }
    let stem_base = strip_core_suffix(&stem).trim_end_matches('_');
    let core = stem_base.trim_end_matches('X');
    if core.is_empty() || core.len() > dev.len() {
        return None;
    }
    let mut score: u32 = 0;
    for (sb, db) in core.bytes().zip(dev.bytes()) {
        if sb == db {
            score += 10;
        } else if sb == b'X' {
            score += 3;
        } else {
            return None;
        }
    }
    let core_bonus = match (dev_core, stem_core) {
        (Some(dc), Some(sc)) if dc == sc => 20,
        _ => 0,
    };
    let trailing_x = stem_base.len().saturating_sub(core.len()) as u32;
    let exactness_tie = 9u32.saturating_sub(trailing_x);
    Some(score * 10 + core_bonus + exactness_tie)
}

fn best_match(device: &str, files: impl Iterator<Item = PathBuf>) -> Option<PathBuf> {
    files
        .filter_map(|path| {
            let stem = path.file_stem()?.to_str()?;
            match_svd_score(device, stem).map(|score| (score, path))
        })
        .max_by_key(|(score, _)| *score)
        .map(|(_, path)| path)
}

pub fn find_in_dir(device: &str, dir: &Path) -> Option<PathBuf> {
    let listing = std::fs::read_dir(dir).ok()?;
    let files = listing.flatten().map(|entry| entry.path()).filter(|path| {
        path.is_file()
            && path
                .extension()
                .is_some_and(|e| e.eq_ignore_ascii_case("svd"))
    });
    best_match(device, files)
}

/// An SVD file for `device` from an installed copy of Ozone, if there is one.
pub fn find_for_device(device: &str) -> Option<PathBuf> {
    let installs = std::fs::read_dir(SEGGER_DIR).ok()?;
    let files: Vec<PathBuf> = installs
        .flatten()
        .filter(|entry| entry.file_name().to_string_lossy().starts_with("Ozone"))
        .filter_map(|entry| std::fs::read_dir(entry.path().join(OZONE_PERIPHERALS)).ok())
        .flat_map(|listing| listing.flatten().map(|file| file.path()))
        .filter(|path| {
            path.extension()
                .is_some_and(|e| e.eq_ignore_ascii_case("svd"))
        })
        .collect();
    best_match(device, files.into_iter())
}

fn find_in_st_tools(device: &str) -> Option<PathBuf> {
    #[cfg(not(windows))]
    let roots = ["/opt/st", "/opt/STMicroelectronics"];
    #[cfg(windows)]
    let roots = [r"C:\ST", r"C:\Program Files\STMicroelectronics"];

    let mut candidates = Vec::new();
    for root in roots {
        let Ok(entries) = std::fs::read_dir(root) else {
            continue;
        };
        for entry in entries.flatten() {
            let p = entry.path();
            for sub in ["STMicroelectronics_CMSIS_SVD", "SVD", "CMSIS/SVD"] {
                let dir = p.join(sub);
                if dir.is_dir() {
                    if let Some(found) = find_in_dir(device, &dir) {
                        candidates.push(found);
                    }
                }
            }
        }
    }
    best_match(device, candidates.into_iter())
}

/// Find an SVD file for `device`, checking Haltd's global SVD cache first,
/// then installed SEGGER Ozone and ST toolchain directories.
pub fn find_for_device_with_cache(device: &str, cache_root: Option<&Path>) -> Option<PathBuf> {
    if let Some(root) = cache_root {
        if let Some(found) = find_in_dir(device, &root.join("svd").join("files")) {
            return Some(found);
        }
        if let Some(found) = find_in_dir(device, &root.join("svd")) {
            return Some(found);
        }
    }
    find_for_device(device).or_else(|| find_in_st_tools(device))
}

/// Official `Open-CMSIS-Pack` repository name for an STM32 device, if recognized.
pub fn stm32_dfp_repo(device: &str) -> Option<&'static str> {
    let dev = normalize_device_name(device);
    if !dev.starts_with("STM32") {
        return None;
    }
    if dev.starts_with("STM32H7R") || dev.starts_with("STM32H7S") {
        return Some("STM32H7RSxx_DFP");
    }
    if dev.starts_with("STM32WBA") {
        return Some("STM32WBAxx_DFP");
    }
    if dev.starts_with("STM32WB") {
        return Some("STM32WBxx_DFP");
    }
    if dev.starts_with("STM32WL") {
        return Some("STM32WLxx_DFP");
    }
    match dev.get(5..7)? {
        "C0" => Some("STM32C0xx_DFP"),
        "F0" => Some("STM32F0xx_DFP"),
        "F1" => Some("STM32F1xx_DFP"),
        "F2" => Some("STM32F2xx_DFP"),
        "F3" => Some("STM32F3xx_DFP"),
        "F4" => Some("STM32F4xx_DFP"),
        "F7" => Some("STM32F7xx_DFP"),
        "G0" => Some("STM32G0xx_DFP"),
        "G4" => Some("STM32G4xx_DFP"),
        "H5" => Some("STM32H5xx_DFP"),
        "H7" => Some("STM32H7xx_DFP"),
        "L0" => Some("STM32L0xx_DFP"),
        "L1" => Some("STM32L1xx_DFP"),
        "L4" => Some("STM32L4xx_DFP"),
        "L5" => Some("STM32L5xx_DFP"),
        "N6" => Some("STM32N6xx_DFP"),
        "U0" => Some("STM32U0xx_DFP"),
        "U3" => Some("STM32U3xx_DFP"),
        "U5" => Some("STM32U5xx_DFP"),
        _ => None,
    }
}

/// Deterministic official raw URLs to try for `device` when directory index is unavailable.
pub fn candidate_remote_urls(device: &str) -> Vec<(String, String)> {
    let dev = normalize_device_name(device);
    let Some(repo) = stm32_dfp_repo(device) else {
        return Vec::new();
    };
    let mut stems = Vec::new();
    if dev.len() >= 9 {
        let p9 = &dev[..9];
        stems.push(p9.to_owned());
        stems.push(format!("{p9}xx"));
        if let Some(last) = p9.chars().last() {
            if let Some(p7) = p9.get(..7) {
                stems.push(format!("{p7}x{last}"));
            }
        }
    }
    if dev.len() >= 7 {
        stems.push(format!("{}xx", &dev[..7]));
    }
    let mut urls = Vec::new();
    for stem in &stems {
        let filename = format!("{stem}.svd");
        urls.push((
            filename.clone(),
            format!("https://raw.githubusercontent.com/Open-CMSIS-Pack/{repo}/main/CMSIS/SVD/{filename}"),
        ));
    }
    for stem in &stems {
        let filename = format!("{stem}.svd");
        urls.push((
            filename.clone(),
            format!("https://raw.githubusercontent.com/cmsis-svd/cmsis-svd-data/main/data/STMicro/{filename}"),
        ));
    }
    urls
}

/// Download an official SVD for `device` via system `curl` and cache it under `<cache_root>/svd/files/`.
/// Intended as an on-demand fallback for CLI/headless callers without requiring TLS crates.
pub fn fetch_and_cache_for_device(device: &str, cache_root: &Path) -> Option<PathBuf> {
    if let Some(local) = find_for_device_with_cache(device, Some(cache_root)) {
        return Some(local);
    }
    let files_dir = cache_root.join("svd").join("files");
    std::fs::create_dir_all(&files_dir).ok()?;
    for (filename, url) in candidate_remote_urls(device) {
        let output = std::process::Command::new("curl")
            .args(["-fsSL", "--connect-timeout", "4", "--max-time", "12", &url])
            .output()
            .ok()?;
        if !output.status.success() {
            continue;
        }
        let Ok(xml) = String::from_utf8(output.stdout) else {
            continue;
        };
        if parse(&xml).is_err() {
            continue;
        }
        let tmp = files_dir.join(format!("{filename}.part.{}", std::process::id()));
        let dest = files_dir.join(&filename);
        if std::fs::write(&tmp, xml.as_bytes()).is_ok() && std::fs::rename(&tmp, &dest).is_ok() {
            return Some(dest);
        }
        let _ = std::fs::remove_file(&tmp);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"<?xml version="1.0"?>
<device>
  <name>TEST</name>
  <size>32</size>
  <access>read-write</access>
  <peripherals>
    <peripheral derivedFrom="UART1">
      <name>UART2</name>
      <baseAddress>0x40001000</baseAddress>
    </peripheral>
    <peripheral>
      <name>UART1</name>
      <description>Serial
        port</description>
      <baseAddress>0x40000000</baseAddress>
      <interrupt><name>USART1</name><value>37</value></interrupt>
      <registers>
        <register>
          <name>SR</name>
          <description>Status</description>
          <addressOffset>0x0</addressOffset>
          <size>16</size>
          <access>read-only</access>
          <fields>
            <field><name>RDY</name><bitOffset>0</bitOffset><bitWidth>1</bitWidth>
              <enumeratedValues>
                <enumeratedValue><name>NO</name><value>0</value></enumeratedValue>
                <enumeratedValue><name>YES</name><value>#1</value></enumeratedValue>
              </enumeratedValues>
            </field>
            <field><name>MODE</name><bitRange>[7:4]</bitRange></field>
            <field><name>TOP</name><lsb>30</lsb><msb>31</msb></field>
          </fields>
        </register>
        <register><name>DR</name><addressOffset>4</addressOffset></register>
        <register>
          <dim>3</dim><dimIncrement>4</dimIncrement>
          <name>CH%s</name><addressOffset>0x10</addressOffset>
        </register>
        <register derivedFrom="SR">
          <name>SR2</name><addressOffset>0x40</addressOffset>
        </register>
        <register>
          <name>CLR</name><addressOffset>0x44</addressOffset><readAction>clear</readAction>
        </register>
        <cluster>
          <name>BLOCK</name><addressOffset>0x100</addressOffset>
          <register><name>A</name><addressOffset>8</addressOffset></register>
        </cluster>
      </registers>
    </peripheral>
    <peripheral>
      <dim>2</dim><dimIncrement>0x100</dimIncrement><dimIndex>A,B</dimIndex>
      <name>GPIO%s</name>
      <baseAddress>0x50000000</baseAddress>
      <registers><register><name>IDR</name><addressOffset>0</addressOffset></register></registers>
    </peripheral>
  </peripherals>
</device>"#;

    fn sample() -> Device {
        parse(SAMPLE).unwrap()
    }

    #[test]
    fn registers_inherit_and_override_properties() {
        let device = sample();
        let uart = device.peripheral("UART1").unwrap();
        assert_eq!(uart.description, "Serial port");
        let status = uart.register("SR").unwrap();
        assert_eq!((status.address, status.bits), (0x4000_0000, 16));
        assert_eq!(status.access, Access::ReadOnly);
        let data = uart.register("DR").unwrap();
        assert_eq!((data.bits, data.access), (32, Access::ReadWrite));
    }

    #[test]
    fn all_three_bit_range_forms_parse() {
        let device = sample();
        let status = device.peripheral("UART1").unwrap().register("SR").unwrap();
        let find = |name| status.fields.iter().find(|f| f.name == name).unwrap();
        assert_eq!((find("RDY").lsb, find("RDY").width), (0, 1));
        assert_eq!((find("MODE").lsb, find("MODE").width), (4, 4));
        assert_eq!((find("TOP").lsb, find("TOP").width), (30, 2));
        assert_eq!(find("RDY").value_name(1), Some("YES"));
        assert_eq!(find("RDY").access, Access::ReadOnly);
    }

    #[test]
    fn peripherals_derive_even_from_later_declarations() {
        let device = sample();
        let second = device.peripheral("UART2").unwrap();
        assert_eq!(second.register("DR").unwrap().address, 0x4000_1004);
        assert_eq!(second.description, "Serial port");
    }

    #[test]
    fn dim_arrays_expand() {
        let device = sample();
        let uart = device.peripheral("UART1").unwrap();
        assert_eq!(uart.register("CH2").unwrap().address, 0x4000_0018);
        assert_eq!(device.peripheral("GPIOB").unwrap().base, 0x5000_0100);
        let idr = device.peripheral("GPIOB").unwrap().register("IDR").unwrap();
        assert_eq!(idr.address, 0x5000_0100);
    }

    #[test]
    fn registers_can_derive_within_a_peripheral() {
        let device = sample();
        let derived = device.peripheral("UART1").unwrap().register("SR2").unwrap();
        assert_eq!(derived.address, 0x4000_0040);
        assert_eq!(derived.bits, 16);
        assert_eq!(derived.fields.len(), 3);
    }

    #[test]
    fn clusters_add_their_offset() {
        let device = sample();
        let inner = device
            .peripheral("UART1")
            .unwrap()
            .register("BLOCK.A")
            .unwrap();
        assert_eq!(inner.address, 0x4000_0108);
    }

    #[test]
    fn reads_with_side_effects_are_flagged() {
        let device = sample();
        let uart = device.peripheral("UART1").unwrap();
        assert!(uart.register("DR").unwrap().read_has_side_effects);
        assert!(uart.register("CLR").unwrap().read_has_side_effects);
        assert!(!uart.register("SR").unwrap().read_has_side_effects);
    }

    #[test]
    fn fields_read_and_write_within_their_bits() {
        let field = Field {
            lsb: 4,
            width: 4,
            ..Field::default()
        };
        assert_eq!(field.mask(), 0xF0);
        assert_eq!(field.extract(0xA5), 0xA);
        assert_eq!(field.insert(0xA5, 3), Some(0x35));
        assert_eq!(field.insert(0xA5, 16), None);
        let whole = Field {
            lsb: 0,
            width: 32,
            ..Field::default()
        };
        assert_eq!(whole.mask(), u32::MAX);
    }

    #[test]
    fn numbers_take_svd_forms() {
        assert_eq!(number("0x10"), Some(16));
        assert_eq!(number("#101"), Some(5));
        assert_eq!(number("2k"), Some(2048));
        assert_eq!(number("12"), Some(12));
        assert_eq!(number("zz"), None);
    }

    #[test]
    fn the_longest_part_number_wins() {
        let files = [
            "STM32F1xx.svd",
            "STM32F103xx.svd",
            "STM32H7xx.svd",
            "STM32H7x3.svd",
            "STM32H743.svd",
            "STM32L4x6.svd",
            "STM32H745_CM7.svd",
            "STM32H745_CM4.svd",
            "nRF52.svd",
        ];
        assert_eq!(
            best_match("stm32f103c8", files.iter().map(PathBuf::from)),
            Some(PathBuf::from("STM32F103xx.svd"))
        );
        assert_eq!(
            best_match("STM32H723VG", files.iter().map(PathBuf::from)),
            Some(PathBuf::from("STM32H7x3.svd"))
        );
        assert_eq!(
            best_match("STM32H743ZI", files.iter().map(PathBuf::from)),
            Some(PathBuf::from("STM32H743.svd"))
        );
        assert_eq!(
            best_match("STM32L476RG", files.iter().map(PathBuf::from)),
            Some(PathBuf::from("STM32L4x6.svd"))
        );
        assert_eq!(
            best_match("STM32H745XI_M7", files.iter().map(PathBuf::from)),
            Some(PathBuf::from("STM32H745_CM7.svd"))
        );
        assert_eq!(best_match("LPC1768", files.iter().map(PathBuf::from)), None);
    }

    #[test]
    fn finds_cached_svd_and_builds_official_urls() {
        let tmp = std::env::temp_dir().join(format!("haltd-svd-test-{}", std::process::id()));
        let files_dir = tmp.join("svd").join("files");
        std::fs::create_dir_all(&files_dir).unwrap();
        let h723 = files_dir.join("STM32H723.svd");
        std::fs::write(&h723, SAMPLE).unwrap();
        assert_eq!(
            find_for_device_with_cache("STM32H723VG", Some(&tmp)),
            Some(h723)
        );
        let urls = candidate_remote_urls("STM32H723VG");
        assert!(urls.iter().any(|(name, url)| {
            name == "STM32H723.svd"
                && url == "https://raw.githubusercontent.com/Open-CMSIS-Pack/STM32H7xx_DFP/main/CMSIS/SVD/STM32H723.svd"
        }));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn invalid_input_is_an_error() {
        assert!(parse("<device>").is_err());
        assert!(parse("<device/>").is_err());
    }

    #[test]
    fn parses_peripheral_interrupt_numbers() {
        let device = sample();
        assert_eq!(
            device.interrupts.get(&37).map(String::as_str),
            Some("USART1")
        );
    }
}
