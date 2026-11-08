//! Ring of samples with a min/max pyramid (DESIGN §5.9).
//!
//! Samples have a global index that only grows. A pyramid level with shift
//! `s` holds one min/max per `2^s` consecutive indices, so the extremes of any
//! index range cost a few dozen lookups however many samples it spans.

const LEVEL_SHIFTS: [u32; 9] = [2, 4, 6, 8, 10, 12, 14, 16, 18];

struct Level {
    shift: u32,
    capacity: usize,
    min: Vec<f64>,
    max: Vec<f64>,
}

impl Level {
    fn new(shift: u32, sample_capacity: usize) -> Self {
        Self {
            shift,
            capacity: (sample_capacity >> shift) + 2,
            min: Vec::new(),
            max: Vec::new(),
        }
    }

    fn push(&mut self, index: u64, value: f64) {
        let bucket = index >> self.shift;
        let slot = (bucket % self.capacity as u64) as usize;
        if slot == self.min.len() {
            self.min.push(f64::INFINITY);
            self.max.push(f64::NEG_INFINITY);
        }
        if index & ((1 << self.shift) - 1) == 0 {
            self.min[slot] = f64::INFINITY;
            self.max[slot] = f64::NEG_INFINITY;
        }
        // f64::min and max ignore NaN, so a NaN sample cannot poison a bucket.
        self.min[slot] = self.min[slot].min(value);
        self.max[slot] = self.max[slot].max(value);
    }

    fn get(&self, bucket: u64) -> (f64, f64) {
        let slot = (bucket % self.capacity as u64) as usize;
        (self.min[slot], self.max[slot])
    }
}

pub struct Ring {
    capacity: usize,
    t: Vec<i64>,
    v: Vec<f64>,
    total: u64,
    levels: Vec<Level>,
}

/// Samples returned from [`Ring::read_since`].
#[derive(Debug, Default, PartialEq)]
pub struct Chunk {
    pub t: Vec<i64>,
    pub v: Vec<f64>,
    /// Pass back as the next cursor.
    pub next: u64,
    /// Samples overwritten before they could be read.
    pub dropped: u64,
}

impl Ring {
    pub fn new(capacity: usize) -> Self {
        assert!(capacity > 0);
        Self {
            capacity,
            t: Vec::new(),
            v: Vec::new(),
            total: 0,
            levels: LEVEL_SHIFTS
                .iter()
                .filter(|shift| (1usize << **shift) <= capacity)
                .map(|shift| Level::new(*shift, capacity))
                .collect(),
        }
    }

    pub fn total(&self) -> u64 {
        self.total
    }

    pub fn len(&self) -> usize {
        self.total.min(self.capacity as u64) as usize
    }

    pub fn is_empty(&self) -> bool {
        self.total == 0
    }

    fn first_index(&self) -> u64 {
        self.total.saturating_sub(self.capacity as u64)
    }

    fn slot(&self, index: u64) -> usize {
        (index % self.capacity as u64) as usize
    }

    pub fn time_at(&self, index: u64) -> i64 {
        self.t[self.slot(index)]
    }

    pub fn value_at(&self, index: u64) -> f64 {
        self.v[self.slot(index)]
    }

    pub fn last_time(&self) -> Option<i64> {
        (self.total > 0).then(|| self.time_at(self.total - 1))
    }

    pub fn first_time(&self) -> Option<i64> {
        (self.total > 0).then(|| self.time_at(self.first_index()))
    }

    /// Append a sample. Time never goes backwards within a series, so a late
    /// timestamp is nudged to just after the previous one.
    pub fn push(&mut self, t: i64, v: f64) {
        let t = self.last_time().map_or(t, |last| t.max(last + 1));
        let slot = self.slot(self.total);
        if slot == self.t.len() {
            self.t.push(t);
            self.v.push(v);
        } else {
            self.t[slot] = t;
            self.v[slot] = v;
        }
        for level in &mut self.levels {
            level.push(self.total, v);
        }
        self.total += 1;
    }

    /// First index in `[from, total)` with time >= `t`.
    pub fn lower_bound(&self, from: u64, t: i64) -> u64 {
        let (mut low, mut high) = (from.max(self.first_index()), self.total);
        while low < high {
            let middle = low + (high - low) / 2;
            if self.time_at(middle) < t {
                low = middle + 1;
            } else {
                high = middle;
            }
        }
        low
    }

    /// Extremes of indices `[low, high)`, using the largest buckets that fit.
    fn min_max(&self, mut low: u64, high: u64) -> (f64, f64) {
        let (mut min, mut max) = (f64::INFINITY, f64::NEG_INFINITY);
        while low < high {
            let bucket = self.levels.iter().rev().find(|level| {
                let size = 1u64 << level.shift;
                low & (size - 1) == 0 && low + size <= high
            });
            match bucket {
                Some(level) => {
                    let (bucket_min, bucket_max) = level.get(low >> level.shift);
                    min = min.min(bucket_min);
                    max = max.max(bucket_max);
                    low += 1 << level.shift;
                }
                None => {
                    let value = self.value_at(low);
                    min = min.min(value);
                    max = max.max(value);
                    low += 1;
                }
            }
        }
        (min, max)
    }

    /// Fill per-column extremes for `columns` equal time slices of `[t0, t1)`.
    /// A column with no finite sample gets NaN.
    pub fn columns(&self, t0: i64, t1: i64, min: &mut [f64], max: &mut [f64]) {
        let columns = min.len() as i128;
        let span = i128::from(t1 - t0);
        let mut index = self.lower_bound(0, t0);
        for column in 0..min.len() {
            let end = t0 + (span * (column as i128 + 1) / columns) as i64;
            let next = self.lower_bound(index, end);
            let (low, high) = if next > index {
                self.min_max(index, next)
            } else {
                (f64::INFINITY, f64::NEG_INFINITY)
            };
            min[column] = if low.is_finite() { low } else { f64::NAN };
            max[column] = if high.is_finite() { high } else { f64::NAN };
            index = next;
        }
    }

    pub fn read_since(&self, cursor: u64, limit: usize) -> Chunk {
        let first = self.first_index();
        let start = cursor.max(first);
        let end = self.total.min(start + limit as u64);
        let mut chunk = Chunk {
            t: Vec::with_capacity((end - start) as usize),
            v: Vec::with_capacity((end - start) as usize),
            next: end,
            dropped: start - cursor.min(start),
        };
        for index in start..end {
            chunk.t.push(self.time_at(index));
            chunk.v.push(self.value_at(index));
        }
        chunk
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn filled(capacity: usize, count: u64) -> Ring {
        let mut ring = Ring::new(capacity);
        for i in 0..count {
            ring.push(i as i64 * 1000, i as f64);
        }
        ring
    }

    fn brute(ring: &Ring, low: u64, high: u64) -> (f64, f64) {
        (low..high)
            .map(|i| ring.value_at(i))
            .fold((f64::INFINITY, f64::NEG_INFINITY), |(a, b), v| {
                (a.min(v), b.max(v))
            })
    }

    #[test]
    fn min_max_matches_brute_force_at_many_ranges() {
        let mut ring = Ring::new(5000);
        // Deterministic but irregular values.
        let mut x = 12345u64;
        for i in 0..8000 {
            x = x
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            ring.push(i * 10, (x >> 40) as f64);
        }
        let first = ring.first_index();
        let mut y = 99u64;
        for _ in 0..2000 {
            y = y
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            let a = first + (y >> 33) % 5000;
            let b = first + (y >> 7) % 5000;
            let (low, high) = (a.min(b), a.max(b) + 1);
            let high = high.min(ring.total());
            assert_eq!(
                ring.min_max(low, high),
                brute(&ring, low, high),
                "{low}..{high}"
            );
        }
    }

    #[test]
    fn columns_cover_a_time_range_with_extremes() {
        let ring = filled(100, 100);
        let (mut min, mut max) = (vec![0.0; 4], vec![0.0; 4]);
        ring.columns(0, 40_000, &mut min, &mut max);
        assert_eq!(min, [0.0, 10.0, 20.0, 30.0]);
        assert_eq!(max, [9.0, 19.0, 29.0, 39.0]);
    }

    #[test]
    fn empty_columns_are_nan() {
        let mut ring = Ring::new(10);
        ring.push(0, 1.0);
        ring.push(30, 2.0);
        let (mut min, mut max) = (vec![0.0; 4], vec![0.0; 4]);
        ring.columns(0, 40, &mut min, &mut max);
        assert_eq!(min[0], 1.0);
        assert!(min[1].is_nan() && max[2].is_nan());
        assert_eq!(max[3], 2.0);
    }

    #[test]
    fn a_nan_sample_does_not_poison_a_column() {
        let mut ring = Ring::new(64);
        for i in 0..16 {
            ring.push(i, if i == 5 { f64::NAN } else { i as f64 });
        }
        let (mut min, mut max) = ([0.0], [0.0]);
        ring.columns(0, 16, &mut min, &mut max);
        assert_eq!((min[0], max[0]), (0.0, 15.0));
    }

    #[test]
    fn old_samples_are_overwritten_and_reported_as_dropped() {
        let ring = filled(10, 25);
        assert_eq!(ring.len(), 10);
        let chunk = ring.read_since(0, 100);
        assert_eq!(chunk.dropped, 15);
        assert_eq!(chunk.v.first(), Some(&15.0));
        assert_eq!(chunk.next, 25);
    }

    #[test]
    fn reading_resumes_at_the_cursor_and_honours_the_limit() {
        let ring = filled(100, 10);
        let first = ring.read_since(0, 4);
        assert_eq!(first.v, [0.0, 1.0, 2.0, 3.0]);
        let second = ring.read_since(first.next, 100);
        assert_eq!(second.v.len(), 6);
        assert_eq!(second.dropped, 0);
    }

    #[test]
    fn time_never_runs_backwards() {
        let mut ring = Ring::new(4);
        ring.push(100, 0.0);
        ring.push(50, 1.0);
        assert_eq!(ring.time_at(1), 101);
    }

    #[test]
    fn queries_outside_the_data_are_empty() {
        let ring = filled(10, 10);
        let (mut min, mut max) = ([0.0; 2], [0.0; 2]);
        ring.columns(1_000_000, 2_000_000, &mut min, &mut max);
        assert!(min[0].is_nan() && max[1].is_nan());
    }
}
