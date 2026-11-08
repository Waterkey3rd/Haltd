//! Mapping HSS record timestamps onto the host timeline (DESIGN §5.6.2).
//!
//! The probe's microsecond counter stops while the core is halted and resumes
//! where it left off, so it cannot be mapped once and for all. Data going quiet
//! is treated as a discontinuity and the next batch anchors afresh. It also
//! runs measurably off true (0.37 % on a V9), so its rate is estimated: only
//! then is the offset nearly constant and read latency separable from drift.

/// Consecutive empty reads that count as the target having stopped: 100 ms at
/// the 10 ms read interval. The probe batches small records, so a couple of
/// empty reads in a row are normal while the target runs.
const QUIET_READS: u32 = 10;
/// Batches per half of the sliding window that finds the true offset. It starts
/// short to converge quickly and grows: read latency swings between 0 and about
/// 11 ms with the library's background fetch, so a long window is needed to see
/// its true minimum.
const FIRST_WINDOW_BATCHES: u32 = 32;
const LONGEST_WINDOW_BATCHES: u32 = 256;
/// Host nanoseconds per probe microsecond, nominally and at the plausible limits.
const NS_PER_US: f64 = 1000.0;
/// How far a clock's real rate may plausibly be from nominal.
const RATE_TOLERANCE: f64 = 0.02;
/// Fraction of each rate measurement applied.
const RATE_GAIN: f64 = 0.25;
/// Shortest baseline for a rate measurement.
const MIN_RATE_BASELINE_NS: f64 = 100_000_000.0;
/// Gaps of at least this many periods mean a record was missed.
const LOSS_THRESHOLD: f64 = 1.75;
/// Records used to learn the real period before loss is judged.
const SETTLING_RECORDS: u32 = 32;
const SETTLING_GAIN: f64 = 1.0 / 8.0;
/// The period estimate follows actual spacing by 1/64 per sample.
const PERIOD_GAIN: f64 = 1.0 / 64.0;

#[derive(Clone, Copy, Debug)]
struct Observation {
    candidate: i64,
    extended_us: i64,
    now_ns: i64,
}

#[derive(Debug)]
pub struct ProbeClock {
    last_raw: Option<u32>,
    extended_us: i64,
    /// Probe time corrected by `rate`, so that changing rate causes no step.
    corrected_ns: f64,
    rate: f64,
    nominal_rate: f64,
    track_loss: bool,
    /// Lowest-latency observation so far in this window.
    window_best: Option<Observation>,
    previous_best: Option<Observation>,
    offset_ns: Option<i64>,
    window: [i64; 2],
    window_batches: u32,
    window_length: u32,
    quiet_reads: u32,
    period_us: f64,
    settling: u32,
    pub records: u64,
    pub lost: u64,
    pub discontinuities: u64,
}

impl ProbeClock {
    pub fn new(period_us: u32) -> Self {
        Self {
            last_raw: None,
            extended_us: 0,
            corrected_ns: 0.0,
            rate: NS_PER_US,
            nominal_rate: NS_PER_US,
            track_loss: true,
            window_best: None,
            previous_best: None,
            offset_ns: None,
            window: [i64::MAX; 2],
            window_batches: 0,
            window_length: FIRST_WINDOW_BATCHES,
            quiet_reads: 0,
            period_us: f64::from(period_us),
            settling: SETTLING_RECORDS,
            records: 0,
            lost: 0,
            discontinuities: 0,
        }
    }

    /// For a target-side counter: `ns_per_tick` nominal, and no expected period.
    pub fn for_ticks(ns_per_tick: f64) -> Self {
        Self {
            rate: ns_per_tick,
            nominal_rate: ns_per_tick,
            track_loss: false,
            ..Self::new(1)
        }
    }

    /// Call after a read that returned no records.
    pub fn note_empty_read(&mut self) {
        self.quiet_reads += 1;
        if self.quiet_reads == QUIET_READS && self.offset_ns.is_some() {
            self.offset_ns = None;
            self.discontinuities += 1;
        }
    }

    /// A gap counts as loss only when it spans a whole extra period. Sampling
    /// that competes for the debug port is late by up to 0.7 periods without
    /// any record having been skipped.
    fn account_gap(&mut self, delta_us: u32) {
        if !self.track_loss || self.offset_ns.is_none() {
            return; // Across a halt, spacing says nothing about loss.
        }
        let delta = f64::from(delta_us);
        let periods = delta / self.period_us;
        if self.settling > 0 {
            self.settling -= 1;
            if periods < LOSS_THRESHOLD {
                self.period_us += (delta - self.period_us) * SETTLING_GAIN;
            }
            return;
        }
        if periods >= LOSS_THRESHOLD {
            self.lost += (periods.round() as u64).saturating_sub(1).max(1);
        } else {
            self.period_us += (delta - self.period_us) * PERIOD_GAIN;
        }
    }

    /// Convert one batch of raw timestamps, read at host time `now_ns`, into
    /// host nanoseconds.
    pub fn convert(&mut self, raw_us: &[u32], now_ns: i64) -> Vec<i64> {
        if raw_us.is_empty() {
            self.note_empty_read();
            return Vec::new();
        }
        self.quiet_reads = 0;
        let mut extended = Vec::with_capacity(raw_us.len());
        for raw in raw_us {
            if let Some(last) = self.last_raw {
                let delta = raw.wrapping_sub(last);
                self.account_gap(delta);
                self.extended_us += i64::from(delta);
                self.corrected_ns += f64::from(delta) * self.rate;
            }
            self.last_raw = Some(*raw);
            self.records += 1;
            extended.push(self.corrected_ns);
        }

        // The newest record was taken at most one read latency before `now`,
        // and latency only ever adds. So the true offset is the lowest seen,
        // over a window short enough to follow drift between the two clocks.
        let candidate = now_ns - self.corrected_ns as i64;
        if self.offset_ns.is_none() {
            self.window = [i64::MAX; 2];
            self.window_batches = 0;
            self.window_length = FIRST_WINDOW_BATCHES;
            self.window_best = None;
            self.previous_best = None;
        }
        self.window[1] = self.window[1].min(candidate);
        if self
            .window_best
            .is_none_or(|best| candidate < best.candidate)
        {
            self.window_best = Some(Observation {
                candidate,
                extended_us: self.extended_us,
                now_ns,
            });
        }
        self.window_batches += 1;
        if self.window_batches == self.window_length {
            self.finish_window();
        }
        let offset = self.window[0].min(self.window[1]);
        self.offset_ns = Some(offset);
        extended
            .into_iter()
            .map(|corrected| corrected as i64 + offset)
            .collect()
    }

    /// Roll the window, refining the rate from the two best observations.
    fn finish_window(&mut self) {
        if let (Some(previous), Some(best)) = (self.previous_best, self.window_best) {
            let baseline = best.extended_us - previous.extended_us;
            if baseline as f64 * self.nominal_rate >= MIN_RATE_BASELINE_NS {
                let measured = (best.now_ns - previous.now_ns) as f64 / baseline as f64;
                let measured = measured.clamp(
                    self.nominal_rate * (1.0 - RATE_TOLERANCE),
                    self.nominal_rate * (1.0 + RATE_TOLERANCE),
                );
                self.rate += (measured - self.rate) * RATE_GAIN;
            }
        }
        self.previous_best = self.window_best.take();
        self.window = [self.window[1], i64::MAX];
        self.window_batches = 0;
        self.window_length = (self.window_length * 2).min(LONGEST_WINDOW_BATCHES);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: i64 = 1_000_000;

    fn batch(start: u32, count: u32) -> Vec<u32> {
        (0..count).map(|i| start.wrapping_add(i * 1000)).collect()
    }

    #[test]
    fn records_land_just_before_the_read_time() {
        let mut clock = ProbeClock::new(1000);
        let times = clock.convert(&batch(5000, 10), 100 * MS);
        assert_eq!(*times.last().unwrap(), 100 * MS);
        assert_eq!(times[0], 91 * MS);
    }

    #[test]
    fn extra_read_latency_does_not_move_the_timeline() {
        let mut clock = ProbeClock::new(1000);
        clock.convert(&batch(0, 10), 100 * MS);
        // The next batch ends 10 ms later but is read 3 ms late.
        let times = clock.convert(&batch(10_000, 10), 113 * MS);
        assert!((times[9] - 110 * MS).abs() < MS / 100, "{}", times[9]);
    }

    #[test]
    fn a_faster_read_pulls_the_offset_down() {
        let mut clock = ProbeClock::new(1000);
        clock.convert(&batch(0, 10), 105 * MS); // First read was slow.
        let times = clock.convert(&batch(10_000, 10), 110 * MS);
        assert_eq!(times[9], 110 * MS);
    }

    #[test]
    fn a_slow_probe_clock_is_followed_closely() {
        // The probe counts 0.05 % slow against the host.
        let mut clock = ProbeClock::new(1000);
        let mut error = 0;
        for index in 0..2000u32 {
            let raw = batch(index * 10 * 1000, 10);
            let host_ms = i64::from(index + 1) * 10;
            let now = host_ms * MS + host_ms * MS / 2000;
            let times = clock.convert(&raw, now);
            error = (now - times[9]).abs();
        }
        assert!(error < MS / 2, "lag {error} ns");
    }

    /// Host time of a record, with `jitter(batch)` extra read latency.
    fn run_with_fast_probe(jitter: impl Fn(u32) -> i64) -> i64 {
        // The probe counts 1003 "microseconds" per real millisecond.
        let mut clock = ProbeClock::new(1003);
        let mut worst = 0;
        for index in 0..3000u32 {
            let raw: Vec<u32> = (0..10).map(|i| (index * 10 + i) * 1003).collect();
            let true_last = i64::from(index * 10 + 9) * MS;
            let times = clock.convert(&raw, true_last + 200_000 + jitter(index));
            if index > 500 {
                worst = worst.max((times[9] - true_last).abs());
            }
        }
        worst
    }

    #[test]
    fn a_fast_probe_clock_does_not_let_latency_through() {
        // Reads are usually prompt but regularly several milliseconds late.
        let worst = run_with_fast_probe(|batch| if batch % 7 == 0 { 8 * MS } else { 0 });
        assert!(worst < MS / 2, "off by {worst} ns");
    }

    #[test]
    fn a_cycle_counter_maps_at_its_clock_rate() {
        // 72 MHz with a 32-bit wrap inside the run.
        let mut clock = ProbeClock::for_ticks(1e9 / 72e6);
        let start = u32::MAX - 36_000_000;
        let mut last = 0;
        for index in 0..2000u32 {
            let ticks = start.wrapping_add(index * 720_000);
            let now = i64::from(index) * 10 * MS + 1000 * MS;
            last = clock.convert(&[ticks], now)[0] - now;
        }
        assert!(last.abs() < MS / 10, "off by {last} ns");
        assert_eq!(clock.lost, 0);
    }

    #[test]
    fn the_counter_may_wrap() {
        let mut clock = ProbeClock::new(1000);
        let times = clock.convert(&[u32::MAX - 999, 0], 10 * MS);
        assert_eq!(times[1] - times[0], MS);
        assert_eq!(clock.lost, 0);
    }

    /// Past the settling records, at `start`, with loss judgement active.
    fn settled(start_us: &mut u32) -> ProbeClock {
        let mut clock = ProbeClock::new(1000);
        // The first batch only anchors the clock.
        clock.convert(&[*start_us], 10 * MS);
        let records = batch(*start_us + 1000, SETTLING_RECORDS + 2);
        clock.convert(&records, 45 * MS);
        *start_us = *records.last().unwrap();
        clock
    }

    #[test]
    fn gaps_count_as_lost_records() {
        let mut last = 0;
        let mut clock = settled(&mut last);
        clock.convert(&[last + 4000], 50 * MS);
        assert_eq!(clock.lost, 3);
    }

    #[test]
    fn a_late_record_that_is_made_up_for_is_not_loss() {
        let mut last = 0;
        let mut clock = settled(&mut last);
        clock.convert(&[last + 1600, last + 2000, last + 3000], 50 * MS);
        assert_eq!(clock.lost, 0);
    }

    #[test]
    fn one_missing_record_with_jitter_is_still_counted() {
        let mut last = 0;
        let mut clock = settled(&mut last);
        clock.convert(&[last + 1950, last + 2950], 50 * MS);
        assert_eq!(clock.lost, 1);
    }

    #[test]
    fn a_stretched_period_is_learned_rather_than_counted_as_loss() {
        let mut clock = ProbeClock::new(1000);
        let mut raw = 0u32;
        for batch_index in 0..100 {
            let times: Vec<u32> = (0..10)
                .map(|_| {
                    raw += 1300;
                    raw
                })
                .collect();
            clock.convert(&times, (batch_index + 1) * 13 * MS);
        }
        assert_eq!(clock.lost, 0);
    }

    #[test]
    fn a_halt_re_anchors_without_reporting_loss() {
        let mut clock = ProbeClock::new(1000);
        clock.convert(&batch(0, 10), 100 * MS);
        for read in 0..QUIET_READS {
            clock.convert(&[], (110 + i64::from(read) * 10) * MS);
        }
        assert_eq!(clock.discontinuities, 1);
        // The probe's counter resumes where it stopped; the host has moved on.
        let times = clock.convert(&batch(10_000, 10), 1000 * MS);
        assert_eq!(times[9], 1000 * MS);
        assert_eq!(clock.lost, 0);
    }

    #[test]
    fn one_empty_read_is_not_a_halt() {
        let mut clock = ProbeClock::new(1000);
        clock.convert(&batch(0, 10), 100 * MS);
        clock.convert(&[], 105 * MS);
        clock.convert(&batch(10_000, 10), 110 * MS);
        assert_eq!(clock.discontinuities, 0);
    }
}
