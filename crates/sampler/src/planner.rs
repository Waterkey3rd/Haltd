//! Fitting signals into HSS blocks and polled read ranges (DESIGN §5.6).

use probe_api::{HssBlock, HSS_RECORD_HEADER_BYTES};

/// Largest gap merged into one HSS block; two blocks cost more than this.
const HSS_MERGE_GAP: u32 = 32;
/// Largest gap merged into one polled read.
const POLL_MERGE_GAP: u32 = 64;
const WORD: u32 = 4;

/// Per-record cost of an extra block, in bytes. From measurements at 4 MHz.
const BLOCK_OVERHEAD: u32 = 8;
/// Record cost sustainable at 1 kHz with a 4 MHz SWD clock.
const BUDGET_AT_4MHZ: u32 = 150;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Signal {
    pub id: u32,
    pub addr: u32,
    pub size: u32,
    /// Higher is kept first when not everything fits.
    pub priority: i32,
    /// Earlier additions are kept first among equal priorities.
    pub order: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HssPlan {
    pub blocks: Vec<HssBlock>,
    /// Signal id and its byte offset within a record.
    pub placements: Vec<(u32, u32)>,
    /// Signals that did not fit.
    pub demoted: Vec<u32>,
}

impl HssPlan {
    pub fn record_size(&self) -> usize {
        HSS_RECORD_HEADER_BYTES
            + self
                .blocks
                .iter()
                .map(|b| b.num_bytes as usize)
                .sum::<usize>()
    }
}

/// Record budget for an interface clock and sample rate. Throughput is what is
/// limited, so a slower rate leaves room for proportionally larger records.
pub fn hss_budget(speed_khz: u32, rate_hz: u32) -> u32 {
    let at_1khz = u64::from(BUDGET_AT_4MHZ) * u64::from(speed_khz.max(1000)) / 4000;
    (at_1khz * 1000 / u64::from(rate_hz.max(1))).min(u64::from(u32::MAX)) as u32
}

/// Merge word-aligned signal extents, returning blocks in address order.
fn merge(signals: &[Signal]) -> Vec<HssBlock> {
    let mut sorted: Vec<(u32, u32)> = signals
        .iter()
        .map(|s| {
            (
                s.addr & !(WORD - 1),
                (s.addr + s.size).next_multiple_of(WORD),
            )
        })
        .collect();
    sorted.sort_unstable();
    let mut blocks: Vec<(u32, u32)> = Vec::new();
    for (start, end) in sorted {
        match blocks.last_mut() {
            Some(last) if start <= last.1 + HSS_MERGE_GAP => last.1 = last.1.max(end),
            _ => blocks.push((start, end)),
        }
    }
    blocks
        .into_iter()
        .map(|(start, end)| HssBlock {
            addr: start,
            num_bytes: end - start,
        })
        .collect()
}

fn cost(blocks: &[HssBlock]) -> u32 {
    blocks
        .iter()
        .map(|block| block.num_bytes + BLOCK_OVERHEAD)
        .sum()
}

pub fn plan_hss(signals: &[Signal], max_blocks: u32, budget: u32) -> HssPlan {
    let mut ranked = signals.to_vec();
    ranked.sort_by_key(|s| (std::cmp::Reverse(s.priority), s.order));
    let mut kept = ranked.len();
    let mut blocks = merge(&ranked);
    while kept > 0 && (blocks.len() as u32 > max_blocks || cost(&blocks) > budget) {
        kept -= 1;
        blocks = merge(&ranked[..kept]);
    }

    let mut placements = Vec::with_capacity(kept);
    for signal in &ranked[..kept] {
        let mut offset = HSS_RECORD_HEADER_BYTES as u32;
        for block in &blocks {
            if signal.addr >= block.addr
                && signal.addr + signal.size <= block.addr + block.num_bytes
            {
                placements.push((signal.id, offset + signal.addr - block.addr));
                break;
            }
            offset += block.num_bytes;
        }
    }
    HssPlan {
        blocks,
        placements,
        demoted: ranked[kept..].iter().map(|s| s.id).collect(),
    }
}

/// A contiguous read covering several signals.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PollRange {
    pub addr: u32,
    pub len: u32,
    /// Signal id and its byte offset within this range.
    pub placements: Vec<(u32, u32)>,
}

pub fn plan_poll(signals: &[Signal]) -> Vec<PollRange> {
    let mut sorted = signals.to_vec();
    sorted.sort_by_key(|s| s.addr);
    let mut ranges: Vec<PollRange> = Vec::new();
    for signal in sorted {
        match ranges.last_mut() {
            Some(range) if signal.addr <= range.addr + range.len + POLL_MERGE_GAP => {
                range.len = range.len.max(signal.addr + signal.size - range.addr);
                range.placements.push((signal.id, signal.addr - range.addr));
            }
            _ => ranges.push(PollRange {
                addr: signal.addr,
                len: signal.size,
                placements: vec![(signal.id, 0)],
            }),
        }
    }
    ranges
}

#[cfg(test)]
mod tests {
    use super::*;

    fn signal(id: u32, addr: u32, size: u32) -> Signal {
        Signal {
            id,
            addr,
            size,
            priority: 0,
            order: u64::from(id),
        }
    }

    const BIG: u32 = 100_000;

    #[test]
    fn neighbours_merge_and_distant_signals_do_not() {
        let plan = plan_hss(
            &[
                signal(1, 0x2000_0000, 4),
                signal(2, 0x2000_0010, 4),
                signal(3, 0x2000_1000, 4),
            ],
            10,
            BIG,
        );
        assert_eq!(
            plan.blocks,
            [
                HssBlock {
                    addr: 0x2000_0000,
                    num_bytes: 0x14
                },
                HssBlock {
                    addr: 0x2000_1000,
                    num_bytes: 4
                }
            ]
        );
        assert!(plan.demoted.is_empty());
    }

    #[test]
    fn offsets_account_for_header_and_earlier_blocks() {
        let plan = plan_hss(
            &[signal(1, 0x2000_1000, 4), signal(2, 0x2000_0004, 2)],
            10,
            BIG,
        );
        let offset = |id| plan.placements.iter().find(|(s, _)| *s == id).unwrap().1;
        assert_eq!(offset(2), 4);
        assert_eq!(offset(1), 8);
        assert_eq!(plan.record_size(), 12);
    }

    #[test]
    fn blocks_are_word_aligned_with_offsets_adjusted() {
        let plan = plan_hss(&[signal(1, 0x2000_0002, 1)], 10, BIG);
        assert_eq!(
            plan.blocks,
            [HssBlock {
                addr: 0x2000_0000,
                num_bytes: 4
            }]
        );
        assert_eq!(plan.placements, [(1, 4 + 2)]);
    }

    #[test]
    fn too_many_blocks_demote_the_newest_signals() {
        let signals: Vec<Signal> = (0..5)
            .map(|i| signal(i, 0x2000_0000 + i * 0x1000, 4))
            .collect();
        let plan = plan_hss(&signals, 3, BIG);
        assert_eq!(plan.blocks.len(), 3);
        assert_eq!(plan.demoted, [3, 4]);
    }

    #[test]
    fn priority_outranks_age() {
        let mut signals: Vec<Signal> = (0..3)
            .map(|i| signal(i, 0x2000_0000 + i * 0x1000, 4))
            .collect();
        signals[2].priority = 1;
        let plan = plan_hss(&signals, 2, BIG);
        assert_eq!(plan.demoted, [1]);
    }

    #[test]
    fn the_byte_budget_limits_what_fits() {
        // 16 contiguous floats cost 64 + 8.
        let signals: Vec<Signal> = (0..16).map(|i| signal(i, 0x2000_0000 + i * 4, 4)).collect();
        assert!(plan_hss(&signals, 10, hss_budget(4000, 1000))
            .demoted
            .is_empty());
        // Sixty do not fit at 4 MHz: 240 + 8 > 150.
        let many: Vec<Signal> = (0..60).map(|i| signal(i, 0x2000_0000 + i * 4, 4)).collect();
        let plan = plan_hss(&many, 10, hss_budget(4000, 1000));
        assert!(!plan.demoted.is_empty());
        assert!(cost(&plan.blocks) <= hss_budget(4000, 1000));
    }

    #[test]
    fn budget_scales_with_the_clock() {
        assert_eq!(hss_budget(4000, 1000), 150);
        assert_eq!(hss_budget(12000, 1000), 450);
    }

    #[test]
    fn a_slower_rate_fits_more() {
        assert_eq!(hss_budget(4000, 100), 1500);
        let many: Vec<Signal> = (0..60).map(|i| signal(i, 0x2000_0000 + i * 4, 4)).collect();
        assert!(!plan_hss(&many, 10, hss_budget(4000, 1000))
            .demoted
            .is_empty());
        assert!(plan_hss(&many, 10, hss_budget(4000, 200))
            .demoted
            .is_empty());
    }

    #[test]
    fn empty_input_yields_an_empty_plan() {
        let plan = plan_hss(&[], 10, BIG);
        assert!(plan.blocks.is_empty() && plan.placements.is_empty());
    }

    #[test]
    fn poll_ranges_merge_within_the_gap() {
        let ranges = plan_poll(&[
            signal(1, 0x100, 4),
            signal(2, 0x100 + 4 + 64, 4),
            signal(3, 0x200, 4),
        ]);
        assert_eq!(ranges.len(), 2);
        assert_eq!(ranges[0].len, 4 + 64 + 4);
        assert_eq!(ranges[0].placements, [(1, 0), (2, 68)]);
        assert_eq!(ranges[1].placements, [(3, 0)]);
    }

    #[test]
    fn overlapping_poll_signals_do_not_shrink_a_range() {
        let ranges = plan_poll(&[signal(1, 0x100, 16), signal(2, 0x104, 4)]);
        assert_eq!(ranges.len(), 1);
        assert_eq!(ranges[0].len, 16);
    }
}
