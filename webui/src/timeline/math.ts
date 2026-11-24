export const DIVISIONS = [
  0.001, 0.002, 0.005, 0.01, 0.02, 0.05, 0.1, 0.2, 0.5, 1, 2, 5, 10, 30,
] as const;
export const DIVISIONS_ON_SCREEN = 10;
export const WHEEL_STEP = 60;
export const MARKERS_AT_DIVISION = 0.02;
export const FOLLOW_LAG_S = 0.03;

export interface SeriesColumnData {
  min: Float64Array;
  max: Float64Array;
}

export interface SeriesStats {
  last: number | null;
  min: number | null;
  max: number | null;
  avg: number | null;
  pp: number | null;
}

export function spanSeconds(division: number): number {
  return division * DIVISIONS_ON_SCREEN;
}

export function stepDivision(current: number, steps: number): number {
  const exact = DIVISIONS.indexOf(current as (typeof DIVISIONS)[number]);
  let idx = exact;
  if (idx < 0) {
    idx = 0;
    let bestDiff = Infinity;
    for (let i = 0; i < DIVISIONS.length; i++) {
      const diff = Math.abs(DIVISIONS[i] - current);
      if (diff < bestDiff) {
        bestDiff = diff;
        idx = i;
      }
    }
  }
  const nextIdx = Math.min(DIVISIONS.length - 1, Math.max(0, idx + steps));
  return DIVISIONS[nextIdx];
}

export function zoomDivisionAround(
  division: number,
  viewEnd: number,
  steps: number,
  fraction: number,
): { division: number; viewEnd: number } {
  const clampedFrac = Math.max(0, Math.min(1, fraction));
  const pivot = viewEnd - spanSeconds(division) * (1 - clampedFrac);
  const nextDivision = stepDivision(division, steps);
  const nextViewEnd = pivot + spanSeconds(nextDivision) * (1 - clampedFrac);
  return { division: nextDivision, viewEnd: nextViewEnd };
}

export function panViewEnd(
  viewEnd: number,
  deltaSeconds: number,
  minEnd = -Infinity,
  maxEnd = Infinity,
): number {
  return Math.min(maxEnd, Math.max(minEnd, viewEnd + deltaSeconds));
}

export function panYRange(
  yMin: number,
  yMax: number,
  dyPx: number,
  heightPx: number,
): { min: number; max: number } {
  const span = yMax - yMin || 1;
  const shift = (dyPx / Math.max(1, heightPx)) * span;
  return { min: yMin + shift, max: yMax + shift };
}

export function fitTimeRange(tMin: number, tMax: number): { division: number; viewEnd: number } {
  const low = Math.min(tMin, tMax);
  const high = Math.max(tMin, tMax);
  const needed = Math.max(0.001, high - low);
  const targetDiv = needed / DIVISIONS_ON_SCREEN;
  let chosen: number = DIVISIONS[DIVISIONS.length - 1];
  for (const d of DIVISIONS) {
    chosen = d;
    if (d >= targetDiv) {
      break;
    }
  }
  const span = spanSeconds(chosen);
  const center = (low + high) / 2;
  let end = center + span / 2;
  if (low >= 0 && end - span < 0) {
    end = Math.max(high, span);
  }
  return { division: chosen, viewEnd: end };
}

export function pxToTime(px: number, width: number, tStart: number, tEnd: number): number {
  if (width <= 0) {
    return tStart;
  }
  const frac = Math.max(0, Math.min(1, px / width));
  return tStart + frac * (tEnd - tStart);
}

export function timeToPx(t: number, width: number, tStart: number, tEnd: number): number {
  const span = tEnd - tStart;
  if (width <= 0 || span <= 0) {
    return 0;
  }
  return ((t - tStart) / span) * width;
}

export function divisionSplits(division: number, min: number, max: number): number[] {
  if (division <= 0 || max < min) {
    return [];
  }
  const splits: number[] = [];
  const epsilon = division * 1e-6;
  const firstTick = Math.ceil((min - epsilon) / division);
  const maxTicks = 200;
  for (let i = 0; i < maxTicks; i++) {
    const tick = firstTick + i;
    const value = tick * division;
    if (value > max + epsilon) {
      break;
    }
    splits.push(value);
  }
  return splits;
}

export function divisionLabels(division: number, splits: number[]): string[] {
  const decimals = division >= 1 ? 0 : Math.ceil(-Math.log10(division) - 1e-9);
  return splits.map((value) => {
    const fixed = value.toFixed(decimals);
    return fixed === "-0" || fixed === "-0.0" || /^-0\.0+$/.test(fixed) ? fixed.slice(1) : fixed;
  });
}

export function formatNumber(val: number | null | undefined, sigDigits = 5): string {
  if (val === null || val === undefined || Number.isNaN(val) || !Number.isFinite(val)) {
    return "—";
  }
  const abs = Math.abs(val);
  if (abs === 0) {
    return "0";
  }
  if (abs >= 1e6 || (abs > 0 && abs < 1e-4)) {
    return val.toExponential(3);
  }
  if (Number.isInteger(val) && abs < 1e6) {
    return String(val);
  }
  return Number(val.toPrecision(sigDigits)).toString();
}

export function formatSeconds(seconds: number): string {
  const size = Math.abs(seconds);
  if (size >= 1) {
    return `${seconds.toFixed(3)} s`;
  }
  if (size >= 1e-3) {
    return `${(seconds * 1e3).toFixed(3)} ms`;
  }
  return `${(seconds * 1e6).toFixed(1)} µs`;
}

export function formatFrequency(deltaSeconds: number): string {
  const abs = Math.abs(deltaSeconds);
  if (abs <= 1e-9) {
    return "—";
  }
  const hz = 1 / abs;
  if (hz >= 1e6) {
    return `${(hz / 1e6).toFixed(2)} MHz`;
  }
  if (hz >= 1e3) {
    return `${(hz / 1e3).toFixed(2)} kHz`;
  }
  return `${hz.toFixed(2)} Hz`;
}

export function valueAtColumns(
  xs: number[],
  data: SeriesColumnData | undefined,
  seconds: number,
): number | null {
  if (!data || xs.length === 0) {
    return null;
  }
  const step = xs.length > 1 ? xs[1] - xs[0] : 1;
  if (step === 0) {
    return null;
  }
  const column = Math.min(xs.length - 1, Math.max(0, Math.round((seconds - xs[0]) / step)));
  for (let distance = 0; distance < xs.length; distance++) {
    for (const index of [column - distance, column + distance]) {
      if (index >= 0 && index < xs.length && !Number.isNaN(data.min[index]) && !Number.isNaN(data.max[index])) {
        return (data.min[index] + data.max[index]) / 2;
      }
    }
  }
  return null;
}

export function computeSeriesStatsFromColumns(data: SeriesColumnData | undefined): SeriesStats {
  if (!data) {
    return { last: null, min: null, max: null, avg: null, pp: null };
  }
  let min = Infinity;
  let max = -Infinity;
  let sum = 0;
  let count = 0;
  let last: number | null = null;
  for (let i = 0; i < data.min.length; i++) {
    const lo = data.min[i];
    const hi = data.max[i];
    if (!Number.isNaN(lo) && !Number.isNaN(hi)) {
      if (lo < min) min = lo;
      if (hi > max) max = hi;
      const mid = (lo + hi) * 0.5;
      sum += mid;
      count++;
      last = mid;
    }
  }
  if (count === 0) {
    return { last: null, min: null, max: null, avg: null, pp: null };
  }
  return {
    last,
    min,
    max,
    avg: sum / count,
    pp: max - min,
  };
}
