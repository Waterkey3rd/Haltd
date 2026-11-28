export const DIVISIONS = [
  0.001, 0.002, 0.005, 0.01, 0.02, 0.05, 0.1, 0.2, 0.5, 1, 2, 5, 10, 30,
] as const;
export const DIVISIONS_ON_SCREEN = 10;
export const WHEEL_STEP = 60;
export const MARKERS_AT_DIVISION = 0.02;
export const FOLLOW_LAG_S = 0.04;

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

/** Formats Y-axis tick values concisely with SI suffixes (Ozone style: 3.2k, 2k, -200). */
export function formatAxisSi(val: number): string {
  if (!Number.isFinite(val)) return "";
  if (val === 0) return "0";
  const abs = Math.abs(val);
  if (abs >= 1e6) {
    return `${Number((val / 1e6).toPrecision(4))}M`;
  }
  if (abs >= 1e3) {
    return `${Number((val / 1e3).toPrecision(4))}k`;
  }
  if (abs < 1e-3) {
    return val.toExponential(1);
  }
  return Number(val.toPrecision(4)).toString();
}

/** Formats numbers with thin-space thousands grouping (e.g. `1 460`, `-67`). */
export function formatSpacedNumber(val: number | null | undefined, sigDigits = 5): string {
  if (val === null || val === undefined || Number.isNaN(val) || !Number.isFinite(val)) {
    return "—";
  }
  if (val === 0) return "0";
  const abs = Math.abs(val);
  if (abs >= 1e7 || (abs > 0 && abs < 1e-4)) {
    return val.toExponential(3);
  }
  const raw = Number.isInteger(val) ? String(val) : Number(val.toPrecision(sigDigits)).toString();
  if (raw.includes("e") || raw.includes("E")) {
    return raw;
  }
  const sign = raw.startsWith("-") ? "-" : "";
  const body = sign ? raw.slice(1) : raw;
  const dotIdx = body.indexOf(".");
  const intPart = dotIdx >= 0 ? body.slice(0, dotIdx) : body;
  const fracPart = dotIdx >= 0 ? body.slice(dotIdx) : "";
  if (intPart.length <= 3) {
    return raw;
  }
  const grouped = intPart.replace(/\B(?=(\d{3})+(?!\d))/g, " ");
  return `${sign}${grouped}${fracPart}`;
}

/** Formats absolute timeline seconds in `+MM:SS.mmm s` or `+SS.mmm s`. */
export function formatOzoneTime(seconds: number): string {
  if (!Number.isFinite(seconds)) return "—";
  const sign = seconds < 0 ? "-" : "+";
  const abs = Math.abs(seconds);
  if (abs >= 60) {
    const mins = Math.floor(abs / 60);
    const rem = abs - mins * 60;
    const whole = Math.floor(rem);
    const ms = Math.floor((rem - whole) * 1000 + 1e-6);
    if (ms >= 1000) {
      return `${sign}${String(mins).padStart(2, "0")}:${String(whole + 1).padStart(2, "0")}.000 s`;
    }
    return `${sign}${String(mins).padStart(2, "0")}:${String(whole).padStart(2, "0")}.${String(ms).padStart(3, "0")} s`;
  }
  return `${sign}${abs.toFixed(3)} s`;
}

/** Formats relative cursor offset with millisecond + sub-millisecond grouping. */
export function formatOzoneDelta(seconds: number): string {
  if (!Number.isFinite(seconds)) return "—";
  const sign = seconds < 0 ? "-" : "+";
  const abs = Math.abs(seconds);
  if (abs < 1e-3) {
    return `${sign}${(abs * 1e6).toFixed(1)} µs`;
  }
  const fixed = abs.toFixed(4);
  const dot = fixed.indexOf(".");
  const intPart = fixed.slice(0, dot);
  const frac = fixed.slice(dot + 1);
  return `${sign}${intPart}.${frac.slice(0, 3)} ${frac.slice(3)} s`;
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
  xs: ArrayLike<number>,
  data: SeriesColumnData | undefined,
  seconds: number,
  isNonUniformXs = false,
): number | null {
  const len = xs.length;
  if (!data || len === 0) {
    return null;
  }
  const minArr = data.min;
  const maxArr = data.max;
  let column: number;
  if (isNonUniformXs) {
    if (len === 1) {
      column = 0;
    } else {
      let low = 0;
      let high = len - 1;
      while (low < high) {
        const mid = (low + high) >>> 1;
        if (xs[mid] < seconds) {
          low = mid + 1;
        } else {
          high = mid;
        }
      }
      column =
        low > 0 && Math.abs(xs[low - 1] - seconds) < Math.abs(xs[low] - seconds)
          ? low - 1
          : low;
    }
  } else {
    const step = len > 1 ? xs[1] - xs[0] : 1;
    if (step === 0) {
      return null;
    }
    column = Math.min(len - 1, Math.max(0, Math.round((seconds - xs[0]) / step)));
  }
  const lo0 = minArr[column];
  const hi0 = maxArr[column];
  if (lo0 === lo0 && hi0 === hi0) {
    return (lo0 + hi0) * 0.5;
  }
  for (let distance = 1; distance < len; distance++) {
    const left = column - distance;
    if (left >= 0) {
      const lo = minArr[left];
      const hi = maxArr[left];
      if (lo === lo && hi === hi) {
        return (lo + hi) * 0.5;
      }
    }
    const right = column + distance;
    if (right < len) {
      const lo = minArr[right];
      const hi = maxArr[right];
      if (lo === lo && hi === hi) {
        return (lo + hi) * 0.5;
      }
    }
    if (left < 0 && right >= len) {
      break;
    }
  }
  return null;
}

export interface QueryLodOptions {
  plotCssWidth: number;
  spanSec: number;
  maxRateHz: number;
  inMotion: boolean;
  hasHaltsInWindow: boolean;
  dpr?: number;
}

export function computeQueryLod(opts: QueryLodOptions): {
  widthPx: number;
  maxRawPoints: number;
} {
  const widthPx = Math.min(1920, Math.max(256, Math.round(opts.plotCssWidth)));
  if (opts.maxRateHz > 0) {
    const expected = Math.ceil(opts.spanSec * opts.maxRateHz);
    const rawCutoff = Math.min(3600, Math.round(opts.plotCssWidth * 1.5));
    const maxRawPoints =
      !opts.hasHaltsInWindow && expected <= rawCutoff
        ? Math.min(4096, Math.max(expected + 64, widthPx))
        : 0;
    return { widthPx: Math.min(widthPx, Math.max(64, expected)), maxRawPoints };
  }
  return { widthPx, maxRawPoints: 0 };
}

export function snapFollowEndNs(rawEndNs: number, spanNs: number, columns: number): number {
  const stepNs = Math.max(1, Math.round(spanNs / Math.max(1, columns)));
  return Math.floor(rawEndNs / stepNs) * stepNs;
}

export function hasMeaningfulSpread(
  min: Float64Array,
  max: Float64Array,
  thresholdVal: number,
): boolean {
  const len = min.length;
  for (let i = 0; i < len; i++) {
    const diff = max[i] - min[i];
    if (diff > thresholdVal) {
      return true;
    }
  }
  return false;
}

export function stabilizeAutoYRange(
  prev: { min: number; max: number } | null,
  dataMin: number,
  dataMax: number,
  padRange: (lo: number, hi: number) => [number, number],
): { min: number; max: number } {
  if (prev !== null && prev.max > prev.min) {
    const curSpan = prev.max - prev.min;
    const dataSpan = Math.max(0, dataMax - dataMin);
    const margin = curSpan * 0.03;
    const fitsInside =
      dataMin >= prev.min + (prev.min === 0 && dataMin >= 0 ? 0 : margin) &&
      dataMax <= prev.max - margin;
    const notTooSmall =
      dataSpan >= curSpan * 0.35 ||
      (dataSpan === 0 && curSpan <= Math.max(2, Math.abs(dataMax) * 0.25));
    if (fitsInside && notTooSmall) {
      return prev;
    }
  }
  const [nMin, nMax] = padRange(dataMin, dataMax);
  return { min: nMin, max: nMax };
}

export function lastValueFromColumns(data: SeriesColumnData | undefined): number | null {
  if (!data) {
    return null;
  }
  const minArr = data.min;
  const maxArr = data.max;
  for (let i = minArr.length - 1; i >= 0; i--) {
    const lo = minArr[i];
    const hi = maxArr[i];
    if (lo === lo && hi === hi) {
      return (lo + hi) * 0.5;
    }
  }
  return null;
}

export function hasAnyNaN(data: Float64Array): boolean {
  for (let i = 0; i < data.length; i++) {
    const v = data[i];
    if (v !== v) {
      return true;
    }
  }
  return false;
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
