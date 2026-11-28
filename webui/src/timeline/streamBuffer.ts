import type uPlot from "uplot";
import type { QueryResult, SampleBatch, SeriesInfo } from "../client";
import type { HaltSpan } from "./dsoPaths";
import { FOLLOW_LAG_S } from "./math";

const INITIAL_RING_CAPACITY = 16_384;
const MAX_RING_CAPACITY = 1 << 20; // 1,048,576 (~17.5 min @ 1 kHz)
const PYRAMID_SHIFTS = [4, 8, 12, 16] as const;

interface PyramidLevel {
  readonly shift: number;
  readonly span: number;
  readonly spanMask: number;
  readonly slotMask: number;
  readonly min: Float64Array;
  readonly max: Float64Array;
}

function createLevel(shift: number, sampleCapacity: number): PyramidLevel {
  const span = 1 << shift;
  // Double the bucket count for power-of-2 bitmask indexing with zero wrap-around aliasing.
  const slots = Math.max(4, (sampleCapacity >> shift) * 2);
  return {
    shift,
    span,
    spanMask: span - 1,
    slotMask: slots - 1,
    min: new Float64Array(slots).fill(Infinity),
    max: new Float64Array(slots).fill(-Infinity),
  };
}

class SeriesRing {
  private capacity = INITIAL_RING_CAPACITY;
  private capMask = INITIAL_RING_CAPACITY - 1;
  private ts = new Float64Array(INITIAL_RING_CAPACITY);
  private vs = new Float64Array(INITIAL_RING_CAPACITY);
  private levels: PyramidLevel[] = PYRAMID_SHIFTS.map((s) =>
    createLevel(s, INITIAL_RING_CAPACITY),
  );
  private baseIndex = 0;
  private total = 0;
  coveredFromSec = Infinity;

  get length(): number {
    return this.total - this.baseIndex;
  }

  atTime(logicalIdx: number): number {
    return this.ts[(this.baseIndex + logicalIdx) & this.capMask];
  }

  atVal(logicalIdx: number): number {
    return this.vs[(this.baseIndex + logicalIdx) & this.capMask];
  }

  firstTime(): number {
    return this.length > 0 ? this.atTime(0) : Infinity;
  }

  lastTime(): number {
    return this.length > 0 ? this.atTime(this.length - 1) : -Infinity;
  }

  lastValue(): number | null {
    return this.length > 0 ? this.atVal(this.length - 1) : null;
  }

  private pushPyramidSample(globalIdx: number, v: number): void {
    const finite = v === v;
    for (let l = 0; l < this.levels.length; l++) {
      const level = this.levels[l];
      const bucket =
        globalIdx < 0x80000000
          ? globalIdx >>> level.shift
          : Math.floor(globalIdx / level.span);
      const bSlot = bucket & level.slotMask;
      if ((globalIdx & level.spanMask) === 0) {
        level.min[bSlot] = Infinity;
        level.max[bSlot] = -Infinity;
      }
      if (finite) {
        if (v < level.min[bSlot]) level.min[bSlot] = v;
        if (v > level.max[bSlot]) level.max[bSlot] = v;
      }
    }
  }

  private rebuildPyramid(): void {
    this.levels = PYRAMID_SHIFTS.map((s) => createLevel(s, this.capacity));
    for (let g = this.baseIndex; g < this.total; g++) {
      this.pushPyramidSample(g, this.vs[g & this.capMask]);
    }
  }

  private growToFit(incoming: number): void {
    const needed = this.length + incoming;
    if (needed <= this.capacity || this.capacity >= MAX_RING_CAPACITY) {
      return;
    }
    let nextCap = this.capacity;
    while (nextCap < needed && nextCap < MAX_RING_CAPACITY) {
      nextCap = Math.min(MAX_RING_CAPACITY, nextCap * 2);
    }
    const len = this.length;
    const nextTs = new Float64Array(nextCap);
    const nextVs = new Float64Array(nextCap);
    for (let i = 0; i < len; i++) {
      const slot = (this.baseIndex + i) & this.capMask;
      nextTs[i] = this.ts[slot];
      nextVs[i] = this.vs[slot];
    }
    this.ts = nextTs;
    this.vs = nextVs;
    this.capacity = nextCap;
    this.capMask = nextCap - 1;
    this.baseIndex = 0;
    this.total = len;
    this.rebuildPyramid();
  }

  append(tsIn: Float64Array, vsIn: Float64Array): void {
    const count = Math.min(tsIn.length, vsIn.length);
    if (count === 0) return;
    this.growToFit(count);
    let evicted = false;
    for (let i = 0; i < count; i++) {
      const t = tsIn[i];
      if (this.length > 0 && t <= this.lastTime()) {
        continue;
      }
      if (this.length >= this.capacity) {
        this.baseIndex++;
        evicted = true;
      }
      const g = this.total;
      const slot = g & this.capMask;
      const v = vsIn[i];
      this.ts[slot] = t;
      this.vs[slot] = v;
      this.pushPyramidSample(g, v);
      this.total++;
    }
    if (evicted && this.length > 0) {
      this.coveredFromSec = Math.max(this.coveredFromSec, this.firstTime());
    }
  }

  seedRaw(rawXs: Float64Array, rawVs: Float64Array, windowStartSec: number): void {
    const count = Math.min(rawXs.length, rawVs.length);
    if (this.length === 0) {
      if (count > 0) {
        this.append(rawXs, rawVs);
      }
      this.coveredFromSec =
        this.length >= MAX_RING_CAPACITY
          ? this.firstTime()
          : Math.min(this.coveredFromSec, windowStartSec);
      return;
    }

    const firstExisting = this.firstTime();
    let prefixLen = 0;
    while (prefixLen < count && rawXs[prefixLen] < firstExisting) {
      prefixLen++;
    }
    if (prefixLen === 0) {
      this.coveredFromSec = Math.min(this.coveredFromSec, windowStartSec);
      return;
    }

    const curLen = this.length;
    const combinedLen = Math.min(MAX_RING_CAPACITY, prefixLen + curLen);
    const keepPrefix = Math.max(0, combinedLen - curLen);
    const prefixStart = prefixLen - keepPrefix;
    let nextCap = this.capacity;
    while (nextCap < combinedLen && nextCap < MAX_RING_CAPACITY) {
      nextCap = Math.min(MAX_RING_CAPACITY, nextCap * 2);
    }
    const nextTs = new Float64Array(nextCap);
    const nextVs = new Float64Array(nextCap);
    let writeIdx = 0;
    for (let i = prefixStart; i < prefixLen; i++) {
      nextTs[writeIdx] = rawXs[i];
      nextVs[writeIdx] = rawVs[i];
      writeIdx++;
    }
    for (let i = 0; i < curLen; i++) {
      const slot = (this.baseIndex + i) & this.capMask;
      nextTs[writeIdx] = this.ts[slot];
      nextVs[writeIdx] = this.vs[slot];
      writeIdx++;
    }
    this.ts = nextTs;
    this.vs = nextVs;
    this.capacity = nextCap;
    this.capMask = nextCap - 1;
    this.baseIndex = 0;
    this.total = writeIdx;
    this.rebuildPyramid();
    this.coveredFromSec =
      prefixStart > 0 ? this.firstTime() : Math.min(this.coveredFromSec, windowStartSec);
  }

  /** Smallest logical index in `[fromLogical, highLogical]` with `atTime(i) >= tSec`. */
  lowerBoundIn(fromLogical: number, highLogical: number, tSec: number): number {
    let lo = fromLogical;
    let hi = highLogical;
    while (lo < hi) {
      const mid = (lo + hi) >>> 1;
      if (this.atTime(mid) < tSec) {
        lo = mid + 1;
      } else {
        hi = mid;
      }
    }
    return lo;
  }

  lowerBound(tSec: number): number {
    return this.lowerBoundIn(0, this.length, tSec);
  }

  /** O(log N) min/max over logical indices `[lowLogical, highLogical)`. */
  rangeMinMax(lowLogical: number, highLogical: number): [number, number] {
    let gLow = this.baseIndex + lowLogical;
    const gHigh = this.baseIndex + highLogical;
    let min = Infinity;
    let max = -Infinity;

    while (gLow < gHigh) {
      let picked: PyramidLevel | null = null;
      for (let l = this.levels.length - 1; l >= 0; l--) {
        const level = this.levels[l];
        if ((gLow & level.spanMask) === 0 && gLow + level.span <= gHigh) {
          picked = level;
          break;
        }
      }
      if (picked !== null) {
        const bucket =
          gLow < 0x80000000
            ? gLow >>> picked.shift
            : Math.floor(gLow / picked.span);
        const bSlot = bucket & picked.slotMask;
        const bMin = picked.min[bSlot];
        const bMax = picked.max[bSlot];
        if (bMin < min) min = bMin;
        if (bMax > max) max = bMax;
        gLow += picked.span;
      } else {
        const v = this.vs[gLow & this.capMask];
        if (v === v) {
          if (v < min) min = v;
          if (v > max) max = v;
        }
        gLow++;
      }
    }

    return [min, max];
  }

  valueAt(tSec: number): number | null {
    const len = this.length;
    if (len === 0) {
      return null;
    }
    const idx = this.lowerBoundIn(0, len, tSec);
    if (idx === 0) {
      return this.atVal(0);
    }
    if (idx >= len) {
      return this.atVal(len - 1);
    }
    const prevIdx = idx - 1;
    const best =
      Math.abs(this.atTime(prevIdx) - tSec) <= Math.abs(this.atTime(idx) - tSec)
        ? prevIdx
        : idx;
    return this.atVal(best);
  }

  windowRange(t0: number, t1: number): { start: number; end: number } {
    const len = this.length;
    if (len === 0) {
      return { start: 0, end: 0 };
    }
    const lo = this.lowerBoundIn(0, len, t0);
    const hi = this.lowerBoundIn(lo, len, t1);
    const start = lo > 0 ? lo - 1 : 0;
    const end = hi < len ? hi + 1 : len;
    return { start, end };
  }
}

class TrackScratch {
  private xsBuf = new Float64Array(0);
  private ysBufs: Float64Array[] = [];
  private nanBufs: Float64Array[] = [];
  private boxedYs: (number | null | undefined)[][] = [];

  xs(len: number): Float64Array {
    if (this.xsBuf.length < len) {
      this.xsBuf = new Float64Array(Math.max(len, 512));
    }
    return this.xsBuf.subarray(0, len);
  }

  ys(slot: number, len: number): Float64Array {
    let buf = this.ysBufs[slot];
    if (!buf || buf.length < len) {
      buf = new Float64Array(Math.max(len, 512));
      this.ysBufs[slot] = buf;
    }
    return buf.subarray(0, len);
  }

  nanArray(slot: number, len: number): Float64Array {
    let buf = this.nanBufs[slot];
    if (!buf || buf.length < len) {
      buf = new Float64Array(Math.max(len, 512));
      buf.fill(NaN);
      this.nanBufs[slot] = buf;
    }
    return buf.subarray(0, len);
  }

  boxed(slot: number, len: number): (number | null | undefined)[] {
    let arr = this.boxedYs[slot];
    if (!arr || arr.length !== len) {
      arr = new Array<number | null | undefined>(len);
      this.boxedYs[slot] = arr;
    }
    return arr;
  }
}

export interface AlignedTrackBuild {
  data: uPlot.AlignedData;
  isRaw: boolean;
  bands: uPlot.Band[];
  bandSig: string;
  seriesShow: boolean[];
  seriesWidth: number[];
  hasData: boolean;
}

function isTimeInsideHalt(halts: readonly HaltSpan[], t: number): boolean {
  for (let i = halts.length - 1; i >= 0; i--) {
    const h = halts[i];
    if (t >= h.from && t <= (h.to ?? Infinity)) return true;
    if (h.to !== null && h.to < t) return false;
  }
  return false;
}

function hasHaltsOverlapping(halts: readonly HaltSpan[], t0: number, t1: number): boolean {
  for (let i = halts.length - 1; i >= 0; i--) {
    const h = halts[i];
    if ((h.to ?? Infinity) < t0) break;
    if (h.from <= t1) return true;
  }
  return false;
}

/**
 * Client-side per-series power-of-2 ring buffer with 4-level min/max pyramid
 * fed by `KIND_SAMPLES` binary pushes and seeded on demand from `KIND_QUERY_RAW`.
 */
export class StreamBuffer {
  private readonly rings = new Map<number, SeriesRing>();
  private readonly trackScratches = new Map<string, TrackScratch>();
  private seeded = false;
  latestTimeSec = 0;
  earliestTimeSec = Infinity;

  isSeeded(): boolean {
    return this.seeded;
  }

  markSeeded(value = true): void {
    this.seeded = value;
  }

  clear(): void {
    this.rings.clear();
    this.trackScratches.clear();
    this.seeded = false;
    this.latestTimeSec = 0;
    this.earliestTimeSec = Infinity;
  }

  retainSeries(ids: Iterable<number>): void {
    const keep = new Set(ids);
    for (const id of this.rings.keys()) {
      if (!keep.has(id)) {
        this.rings.delete(id);
      }
    }
    if (keep.size === 0) {
      this.seeded = false;
    }
    this.recomputeBounds();
  }

  private recomputeBounds(): void {
    let latest = 0;
    let earliest = Infinity;
    for (const ring of this.rings.values()) {
      if (ring.length > 0) {
        latest = Math.max(latest, ring.lastTime());
        earliest = Math.min(earliest, ring.firstTime());
      }
    }
    this.latestTimeSec = latest;
    this.earliestTimeSec = earliest;
  }

  private getScratch(trackKey: string): TrackScratch {
    let s = this.trackScratches.get(trackKey);
    if (!s) {
      s = new TrackScratch();
      this.trackScratches.set(trackKey, s);
    }
    return s;
  }

  appendBatches(batches: readonly SampleBatch[]): void {
    for (const batch of batches) {
      let ring = this.rings.get(batch.seriesId);
      if (!ring) {
        ring = new SeriesRing();
        this.rings.set(batch.seriesId, ring);
      }
      ring.append(batch.ts, batch.vs);
      if (ring.length > 0) {
        this.latestTimeSec = Math.max(this.latestTimeSec, ring.lastTime());
        this.earliestTimeSec = Math.min(this.earliestTimeSec, ring.firstTime());
      }
    }
  }

  seedFromRawResult(result: QueryResult, queriedIds?: readonly number[]): void {
    if (!result.raw || !result.rawXs) {
      return;
    }
    const windowStartSec = result.t0 / 1e9;
    const rawXs = result.rawXs;
    const ids = queriedIds ?? [...result.series.keys()];
    for (const id of ids) {
      let ring = this.rings.get(id);
      if (!ring) {
        ring = new SeriesRing();
        this.rings.set(id, ring);
      }
      const data = result.series.get(id);
      if (data && rawXs.length > 0) {
        ring.seedRaw(rawXs, data.max, windowStartSec);
      } else {
        ring.coveredFromSec = Math.min(ring.coveredFromSec, windowStartSec);
      }
    }
    this.seeded = true;
    this.recomputeBounds();
  }

  canServe(seriesIds: readonly number[], t0: number, historyStart: number): boolean {
    if (seriesIds.length === 0) {
      return false;
    }
    const needFrom = Math.max(t0, historyStart);
    for (const id of seriesIds) {
      const ring = this.rings.get(id);
      if (!ring) {
        return false;
      }
      const coveredFrom = Math.min(ring.coveredFromSec, ring.firstTime());
      if (coveredFrom > needFrom + 0.005) {
        return false;
      }
    }
    return true;
  }

  valueAt(seriesId: number, tSec: number): number | null {
    return this.rings.get(seriesId)?.valueAt(tSec) ?? null;
  }

  lastValue(seriesId: number): number | null {
    return this.rings.get(seriesId)?.lastValue() ?? null;
  }

  buildAlignedTrackData(
    members: readonly SeriesInfo[],
    visibleFlags: readonly boolean[],
    t0: number,
    t1: number,
    plotCssWidth: number,
    dpr: number,
    halts: readonly HaltSpan[],
    showBands: boolean,
    colorOfMember: (member: SeriesInfo) => string,
    bandAlphaHex: string,
    trackKey = "__default__",
  ): AlignedTrackBuild {
    const scratch = this.getScratch(trackKey);
    const spans = members.map((m, idx) => {
      const ring = this.rings.get(m.seriesId);
      const visible = visibleFlags[idx];
      if (!visible || !ring || ring.length === 0) {
        return { ring: undefined, start: 0, end: 0, count: 0 };
      }
      const range = ring.windowRange(t0, t1);
      return { ring, start: range.start, end: range.end, count: range.end - range.start };
    });

    let maxCount = 0;
    let nonEmptySpans = 0;
    for (const s of spans) {
      if (s.count > 0) {
        nonEmptySpans++;
        if (s.count > maxCount) maxCount = s.count;
      }
    }

    const slotCount = showBands ? members.length * 2 : members.length;
    const seriesShow = new Array<boolean>(slotCount).fill(false);
    const seriesWidth = new Array<number>(slotCount).fill(1.05);

    if (nonEmptySpans === 0) {
      const emptyXs = scratch.xs(2);
      emptyXs[0] = t0;
      emptyXs[1] = t1;
      const columns: (Float64Array | (number | null | undefined)[])[] = [];
      for (let i = 0; i < slotCount; i++) {
        if (showBands) {
          const arr = scratch.boxed(i, 2);
          arr[0] = undefined;
          arr[1] = undefined;
          columns.push(arr);
        } else {
          columns.push(scratch.nanArray(i, 2));
        }
      }
      return {
        data: [emptyXs, ...columns],
        isRaw: true,
        bands: [],
        bandSig: "",
        seriesShow,
        seriesWidth,
        hasData: false,
      };
    }

    const rawLimit = Math.min(2400, Math.max(600, Math.round(plotCssWidth * 0.85)));

    if (maxCount <= rawLimit && !showBands) {
      let refSpan: (typeof spans)[number] | null = null;
      let sharedTimestamps = true;
      for (const s of spans) {
        if (s.count === 0 || !s.ring) continue;
        if (!refSpan || !refSpan.ring) {
          refSpan = s;
          continue;
        }
        if (s.count !== refSpan.count) {
          sharedTimestamps = false;
          break;
        }
        const refRing = refSpan.ring;
        const n = s.count;
        if (
          s.ring.atTime(s.start) !== refRing.atTime(refSpan.start) ||
          s.ring.atTime(s.start + n - 1) !== refRing.atTime(refSpan.start + n - 1)
        ) {
          sharedTimestamps = false;
          break;
        }
        for (let k = 1; k < n - 1; k++) {
          if (s.ring.atTime(s.start + k) !== refRing.atTime(refSpan.start + k)) {
            sharedTimestamps = false;
            break;
          }
        }
        if (!sharedTimestamps) break;
      }

      if (sharedTimestamps && refSpan && refSpan.ring) {
        const refRing = refSpan.ring;
        const n = refSpan.count;
        const xs = scratch.xs(n);
        for (let k = 0; k < n; k++) {
          xs[k] = refRing.atTime(refSpan.start + k);
        }
        const yCols: Float64Array[] = [];
        for (let mIdx = 0; mIdx < members.length; mIdx++) {
          const s = spans[mIdx];
          seriesShow[mIdx] = visibleFlags[mIdx];
          if (s.count > 0 && s.ring) {
            const vals = scratch.ys(mIdx, n);
            for (let k = 0; k < n; k++) {
              vals[k] = s.ring.atVal(s.start + k);
            }
            yCols.push(vals);
          } else {
            yCols.push(scratch.nanArray(mIdx, n));
          }
        }
        return {
          data: [xs, ...yCols],
          isRaw: true,
          bands: [],
          bandSig: "",
          seriesShow,
          seriesWidth,
          hasData: true,
        };
      }

      // Mixed-rate tracks: merge timestamps chronologically into reusable Float64Arrays (NaN on missing).
      const cursors = spans.map((s) => s.start);
      let totalMergedCap = 0;
      for (const s of spans) totalMergedCap += s.count;
      const xsFull = scratch.xs(totalMergedCap);
      const yFulls = members.map((_, mIdx) => scratch.ys(mIdx, totalMergedCap));
      let outLen = 0;
      for (;;) {
        let minT = Infinity;
        for (let mIdx = 0; mIdx < spans.length; mIdx++) {
          const s = spans[mIdx];
          if (s.ring && cursors[mIdx] < s.end) {
            const t = s.ring.atTime(cursors[mIdx]);
            if (t < minT) minT = t;
          }
        }
        if (minT === Infinity) break;
        xsFull[outLen] = minT;
        for (let mIdx = 0; mIdx < spans.length; mIdx++) {
          const s = spans[mIdx];
          if (s.ring && cursors[mIdx] < s.end && Math.abs(s.ring.atTime(cursors[mIdx]) - minT) <= 1e-9) {
            yFulls[mIdx][outLen] = s.ring.atVal(cursors[mIdx]);
            cursors[mIdx]++;
          } else {
            yFulls[mIdx][outLen] = NaN;
          }
        }
        outLen++;
      }
      const yCols = yFulls.map((arr, mIdx) => {
        seriesShow[mIdx] = visibleFlags[mIdx];
        return arr.subarray(0, outLen);
      });
      return {
        data: [xsFull.subarray(0, outLen), ...yCols],
        isRaw: true,
        bands: [],
        bandSig: "",
        seriesShow,
        seriesWidth,
        hasData: outLen > 0,
      };
    }

    // O(log N) Pyramid Bucketed Mode aligned to world-time grid
    const cols = Math.min(1920, Math.max(320, Math.round(plotCssWidth * Math.min(2, dpr))));
    const spanSec = Math.max(1e-9, t1 - t0);
    const step = spanSec / cols;
    const gridStart = Math.floor(t0 / step) * step;
    const totalCols = cols + 2;

    if (!showBands) {
      const totalPts = totalCols * 2;
      const xs = scratch.xs(totalPts);
      for (let c = 0; c < totalCols; c++) {
        xs[c * 2] = gridStart + (c + 0.25) * step;
        xs[c * 2 + 1] = gridStart + (c + 0.75) * step;
      }
      const yCols: Float64Array[] = [];
      for (let mIdx = 0; mIdx < members.length; mIdx++) {
        const s = spans[mIdx];
        seriesShow[mIdx] = visibleFlags[mIdx];
        if (s.count === 0 || !s.ring) {
          yCols.push(scratch.nanArray(mIdx, totalPts));
          continue;
        }
        const ring = s.ring;
        const arr = scratch.ys(mIdx, totalPts);
        let idx = ring.lowerBoundIn(s.start, s.end, gridStart);
        for (let c = 0; c < totalCols; c++) {
          const bEnd = gridStart + (c + 1) * step;
          const nextIdx =
            idx < s.end && ring.atTime(idx) < bEnd
              ? ring.lowerBoundIn(idx, s.end, bEnd)
              : idx;
          const outPos = c * 2;
          const nInCol = nextIdx - idx;
          if (nInCol <= 0) {
            arr[outPos] = NaN;
            arr[outPos + 1] = NaN;
          } else if (nInCol === 1) {
            const v = ring.atVal(idx);
            arr[outPos] = v;
            arr[outPos + 1] = v;
          } else if (nInCol === 2) {
            arr[outPos] = ring.atVal(idx);
            arr[outPos + 1] = ring.atVal(idx + 1);
          } else {
            const [minVal, maxVal] = ring.rangeMinMax(idx, nextIdx);
            arr[outPos] = minVal;
            arr[outPos + 1] = maxVal;
          }
          idx = nextIdx;
        }
        yCols.push(arr);
      }
      return {
        data: [xs, ...yCols],
        isRaw: false,
        bands: [],
        bandSig: "",
        seriesShow,
        seriesWidth,
        hasData: true,
      };
    }

    // Explicit showBands = true mode (2 uPlot series + uPlot.Band fill)
    const hasHalts = hasHaltsOverlapping(halts, gridStart, gridStart + totalCols * step);
    const xs = scratch.xs(totalCols);
    for (let c = 0; c < totalCols; c++) {
      xs[c] = gridStart + (c + 0.5) * step;
    }
    const yCols: (number | null | undefined)[][] = [];
    const bands: uPlot.Band[] = [];
    const sigParts: string[] = [];
    for (let mIdx = 0; mIdx < members.length; mIdx++) {
      const s = spans[mIdx];
      const maxArr = scratch.boxed(mIdx * 2, totalCols);
      const minArr = scratch.boxed(mIdx * 2 + 1, totalCols);
      let hasSpread = false;
      if (s.count > 0 && s.ring) {
        const ring = s.ring;
        let idx = ring.lowerBoundIn(s.start, s.end, gridStart);
        for (let c = 0; c < totalCols; c++) {
          const bStart = gridStart + c * step;
          const bEnd = gridStart + (c + 1) * step;
          const nextIdx =
            idx < s.end && ring.atTime(idx) < bEnd
              ? ring.lowerBoundIn(idx, s.end, bEnd)
              : idx;
          if (nextIdx <= idx) {
            const halted = hasHalts && isTimeInsideHalt(halts, (bStart + bEnd) * 0.5);
            maxArr[c] = halted ? null : undefined;
            minArr[c] = halted ? null : undefined;
          } else {
            const [minVal, maxVal] = ring.rangeMinMax(idx, nextIdx);
            if (minVal <= maxVal) {
              maxArr[c] = maxVal;
              minArr[c] = minVal;
              if (maxVal > minVal) hasSpread = true;
            } else {
              maxArr[c] = undefined;
              minArr[c] = undefined;
            }
          }
          idx = nextIdx;
        }
      } else {
        maxArr.fill(undefined);
        minArr.fill(undefined);
      }
      const visible = visibleFlags[mIdx];
      seriesShow[mIdx * 2] = visible;
      seriesShow[mIdx * 2 + 1] = visible && hasSpread;
      seriesWidth[mIdx * 2] = 1.05;
      seriesWidth[mIdx * 2 + 1] = 1;
      if (!hasSpread) minArr.fill(undefined);
      yCols.push(maxArr, minArr);
      if (visible && hasSpread) {
        const color = colorOfMember(members[mIdx]);
        bands.push({
          series: [1 + mIdx * 2, 2 + mIdx * 2],
          fill: `${color.slice(0, 7)}${bandAlphaHex}`,
        });
        sigParts.push(`${mIdx}:${color}`);
      }
    }

    return {
      data: [xs, ...yCols],
      isRaw: false,
      bands,
      bandSig: sigParts.join(","),
      seriesShow,
      seriesWidth,
      hasData: true,
    };
  }
}

/**
 * Phase-locked live clock that advances `viewEnd` smoothly on each display VSync
 * without quantizing to data arrival ticks or jumping across network jitter.
 */
export class LiveClockPll {
  private viewEndSec = 0;
  private lastFrameMs = 0;
  private active = false;

  reset(): void {
    this.active = false;
    this.lastFrameMs = 0;
  }

  step(
    nowSec: number,
    frameTimeMs: number,
    latestStreamTimeSec: number,
    hasFastHss: boolean,
  ): number {
    let desired = nowSec - FOLLOW_LAG_S;
    if (hasFastHss && latestStreamTimeSec > 0 && Math.abs(desired - latestStreamTimeSec) < 0.12) {
      desired = Math.min(desired, latestStreamTimeSec - 0.002);
    }

    const frameDtSec = this.lastFrameMs > 0 ? (frameTimeMs - this.lastFrameMs) / 1000 : 0;
    this.lastFrameMs = frameTimeMs;

    if (
      !this.active ||
      frameDtSec <= 0 ||
      frameDtSec > 0.12 ||
      Math.abs(desired - this.viewEndSec) > 0.2
    ) {
      this.viewEndSec = desired;
      this.active = true;
      return this.viewEndSec;
    }

    const error = desired - this.viewEndSec;
    const gain = Math.max(0.85, Math.min(1.15, 1 + error * 6));
    this.viewEndSec += frameDtSec * gain;
    return this.viewEndSec;
  }
}
