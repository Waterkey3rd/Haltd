import uPlot from "uplot";
import type { QueryResult, SeriesInfo } from "../client";
import {
  drawSharedAxis,
  drawTrackOverlay,
  invalidateAxisSizeCache,
  updateCursorInspector,
} from "./cursors";
import { drawDsoSeries, nullPathBuilder } from "./dsoPaths";
import { attachTrackInteractions, type InteractionCallbacks } from "./interactions";
import {
  describeModeBadge,
  renderTrackLegendHeader,
  renderTrackLegendSignals,
  updateLegendReadouts,
  type LegendCallbacks,
} from "./legend";
import {
  divisionSplits,
  formatAxisSi,
  formatNumber,
  hasAnyNaN,
  hasMeaningfulSpread,
  MARKERS_AT_DIVISION,
  spanSeconds,
  stabilizeAutoYRange,
} from "./math";
import { drawOverview, invalidateOverviewSizeCache } from "./overview";
import {
  colorOf,
  DEFAULT_TRACK_HEIGHT,
  effectiveGroupOf,
  getGroupYScale,
  isSeriesVisible,
  MAX_TRACK_HEIGHT,
  MIN_TRACK_HEIGHT,
  NS,
  saveTrackHeights,
  syncHoverTimeFromRelX,
  toHexColor,
  withAlpha,
  Y_AXIS_SIZE,
  type TimelineState,
  type TrackRow,
} from "./state";

let trackAbortControllers: AbortController[] = [];
let cachedTracksScroll: HTMLElement | null = null;
let cachedSharedAxisCanvas: HTMLCanvasElement | null = null;
let scrollSyncState: TimelineState | null = null;
let scrollListenerAttached = false;

function ensureScrollRectSync(state: TimelineState): void {
  scrollSyncState = state;
  if (scrollListenerAttached) return;
  const el = getTracksScroll();
  if (!el) return;
  scrollListenerAttached = true;
  el.addEventListener(
    "scroll",
    () => {
      if (!scrollSyncState) return;
      for (const track of scrollSyncState.tracks) {
        track.overRect = null;
      }
    },
    { passive: true },
  );
}

function getTracksScroll(): HTMLElement | null {
  if (!cachedTracksScroll) {
    cachedTracksScroll = document.getElementById("tracks-scroll");
  }
  return cachedTracksScroll;
}

function getSharedAxisCanvas(): HTMLCanvasElement | null {
  if (!cachedSharedAxisCanvas) {
    cachedSharedAxisCanvas = document.getElementById("shared-axis-canvas") as HTMLCanvasElement | null;
  }
  return cachedSharedAxisCanvas;
}

export function isHalted(state: TimelineState, seconds: number): boolean {
  for (let index = state.halts.length - 1; index >= 0; index--) {
    const halt = state.halts[index];
    if (seconds >= halt.from && seconds <= (halt.to ?? Infinity)) return true;
    if (halt.to !== null && halt.to < seconds) return false;
  }
  return false;
}

export function hasHaltsInRange(state: TimelineState, start: number, end: number): boolean {
  for (let i = state.halts.length - 1; i >= 0; i--) {
    const halt = state.halts[i];
    const hEnd = halt.to ?? Infinity;
    if (hEnd < start) {
      break;
    }
    if (halt.from <= end) {
      return true;
    }
  }
  return false;
}

function populateGapMask(
  state: TimelineState,
  start: number,
  end: number,
  step: number,
  columns: number,
): Uint8Array | null {
  if (columns <= 0 || state.halts.length === 0 || step <= 0 || !hasHaltsInRange(state, start, end)) {
    return null;
  }
  if (state.scratchGaps.length !== columns) {
    state.scratchGaps = new Uint8Array(columns);
  } else {
    state.scratchGaps.fill(0);
  }
  const mask = state.scratchGaps;
  for (let i = state.halts.length - 1; i >= 0; i--) {
    const halt = state.halts[i];
    const hEnd = halt.to ?? end;
    if (hEnd < start) {
      break;
    }
    if (halt.from > end) {
      continue;
    }
    const c0 = Math.max(0, Math.ceil((halt.from - start) / step));
    const c1 = Math.min(columns - 1, Math.floor((hEnd - start) / step));
    if (c1 >= c0) {
      mask.fill(1, c0, c1 + 1);
    }
  }
  return mask;
}

function emptyUndefined(track: TrackRow, slotIndex: number, length: number): (number | null | undefined)[] {
  let scratch = track.scratchSeries[slotIndex];
  if (!scratch || scratch.length !== length) {
    scratch = new Array<number | null | undefined>(length);
    track.scratchSeries[slotIndex] = scratch;
  }
  scratch.fill(undefined);
  return scratch;
}

function getScratchFloat(track: TrackRow, slotIndex: number, length: number): Float64Array {
  let buf = track.scratchFloatSeries[slotIndex];
  if (!buf || buf.length < length) {
    buf = new Float64Array(Math.max(length, 512));
    track.scratchFloatSeries[slotIndex] = buf;
  }
  return buf.subarray(0, length);
}

function emptyNanFloat(track: TrackRow, slotIndex: number, length: number): Float64Array {
  const buf = getScratchFloat(track, slotIndex, length);
  buf.fill(NaN);
  return buf;
}

let scratchQueryXs2 = new Float64Array(0);
function getScratchQueryXs2(length: number): Float64Array {
  if (scratchQueryXs2.length < length) {
    scratchQueryXs2 = new Float64Array(Math.max(length, 512));
  }
  return scratchQueryXs2.subarray(0, length);
}

function toSeriesArray(
  track: TrackRow,
  slotIndex: number,
  data: Float64Array | undefined,
  columns: number,
  gaps: Uint8Array | null,
): Float64Array | (number | null | undefined)[] {
  if (data && !hasAnyNaN(data)) {
    return data;
  }
  let scratch = track.scratchSeries[slotIndex];
  if (!scratch || scratch.length !== columns) {
    scratch = new Array<number | null | undefined>(columns);
    track.scratchSeries[slotIndex] = scratch;
  }
  if (!data) {
    scratch.fill(null);
    return scratch;
  }
  for (let i = 0; i < columns; i++) {
    const v = data[i];
    scratch[i] = v === v ? v : gaps !== null && gaps[i] !== 0 ? null : undefined;
  }
  return scratch;
}

export function destroyTracks(state: TimelineState): void {
  for (const ac of trackAbortControllers) {
    ac.abort();
  }
  trackAbortControllers = [];
  for (const track of state.tracks) {
    track.plot.destroy();
    track.element.remove();
  }
  state.tracks = [];
}

export function syncTrackYControls(state: TimelineState, track: TrackRow): void {
  const cfg = getGroupYScale(state, track.name);
  track.yAutoBtn.textContent = cfg.auto ? "Auto Y" : "Lock Y";
  track.yAutoBtn.setAttribute("aria-pressed", String(!cfg.auto));
  track.yRangeWrap.hidden = cfg.auto;
  if (!cfg.auto) {
    if (document.activeElement !== track.yMinInput) {
      track.yMinInput.value = formatNumber(cfg.min, 4);
    }
    if (document.activeElement !== track.yMaxInput) {
      track.yMaxInput.value = formatNumber(cfg.max, 4);
    }
  }
}

export function applyTrackYScale(state: TimelineState, track: TrackRow): void {
  const cfg = getGroupYScale(state, track.name);
  syncTrackYControls(state, track);
  if (!cfg.auto && cfg.max > cfg.min) {
    track.plot.setScale("y", { min: cfg.min, max: cfg.max });
  }
}

export function updateTrackSelectionHighlight(state: TimelineState): void {
  for (const track of state.tracks) {
    track.element.classList.toggle("is-selected", state.selectedTrack === track.name);
    for (const [id, card] of track.legendCards.entries()) {
      card.classList.toggle("is-selected", state.selectedSeriesId === id);
    }
  }
}

export function applySeriesFilter(state: TimelineState): void {
  const filter = state.seriesFilter.trim().toLowerCase();
  for (const track of state.tracks) {
    for (const item of track.members) {
      const dom = track.legendSignals.get(item.seriesId);
      if (!dom) continue;
      if (!filter) {
        dom.row.classList.remove("is-filtered-out");
      } else {
        const hay = `${item.name} ${item.root} ${item.group ?? ""} ${item.actualMode}`.toLowerCase();
        dom.row.classList.toggle("is-filtered-out", !hay.includes(filter));
      }
    }
  }
}

export function syncTracksAppearance(state: TimelineState): void {
  const bandAlpha = state.canvasTheme.isLight ? "24" : "33";
  for (const track of state.tracks) {
    for (let mIdx = 0; mIdx < track.members.length; mIdx++) {
      const item = track.members[mIdx];
      const visible = isSeriesVisible(state, item);
      const color = colorOf(state, item);
      const dom = track.legendSignals.get(item.seriesId);
      if (dom) {
        dom.row.classList.toggle("is-hidden", !visible);
        dom.checkbox.checked = visible;
        dom.checkbox.style.accentColor = color;
        dom.swatch.value = toHexColor(color);
        dom.soloBtn.classList.toggle("active", state.soloSeriesId === item.seriesId);
      }
    }
  }

  const ids = state.tracks.flatMap((t) => t.members.map((m) => m.seriesId));
  const t0 = state.viewEnd - spanSeconds(state.division);
  if (state.tracks.length > 0 && state.streamBuffer.canServe(ids, t0, state.historyStart)) {
    drawStreamFrame(state);
    return;
  }
  if (state.lastColumns) {
    applyCachedWaveforms(state);
    return;
  }

  for (const track of state.tracks) {
    const stride = track.hasBandSeries ? 2 : 1;
    track.plot.batch(() => {
      const nextBands: uPlot.Band[] = [];
      const sigParts: string[] = [];
      for (let mIdx = 0; mIdx < track.members.length; mIdx++) {
        const item = track.members[mIdx];
        const visible = isSeriesVisible(state, item);
        const color = colorOf(state, item);
        const maxIdx = 1 + mIdx * stride;
        const sMax = track.plot.series[maxIdx];
        if (sMax) sMax.show = visible;
        if (track.hasBandSeries) {
          const minIdx = maxIdx + 1;
          const sMin = track.plot.series[minIdx];
          if (sMin) {
            sMin.show = state.showBands && visible;
            sMin.width = state.showBands ? 1 : 1.05;
          }
          if (state.showBands && visible) {
            nextBands.push({
              series: [maxIdx, minIdx],
              fill: withAlpha(color, bandAlpha),
            });
            sigParts.push(`${mIdx}:${color}`);
          }
        }
      }
      const nextSig = sigParts.join(",");
      if (nextSig !== track.bandSig) {
        track.plot.delBand();
        for (const band of nextBands) {
          track.plot.addBand(band);
        }
        track.bandSig = nextSig;
      }
      track.plot.redraw(true);
    });
  }
  updateLegendReadouts(state);
  updateCursorInspector(state);
}

/** Redraws only track overlay canvases (crosshair, intersection readouts) for 0ms hover tracking. */
export function redrawTrackOverlaysOnly(state: TimelineState): void {
  for (const track of state.tracks) {
    if (getGroupYScale(state, track.name).collapsed) continue;
    drawTrackOverlay(state, track);
  }
}

/**
 * Redraws only lightweight interactive overlay canvases (crosshair, cursors, pins,
 * shared axis, and overview) without repainting uPlot waveform paths or grids.
 */
export function redrawAllOverlays(state: TimelineState): void {
  redrawTrackOverlaysOnly(state);
  const axisCanvas = getSharedAxisCanvas();
  if (axisCanvas) {
    drawSharedAxis(axisCanvas, state);
  }
  drawOverview(state);
}

function computeTrackViewportHeights(state: TimelineState): void {
  const scrollEl = getTracksScroll();
  const scrollH = scrollEl?.clientHeight ?? 0;
  const expanded: TrackRow[] = [];
  let fixedChrome = 0;

  for (const track of state.tracks) {
    const cfg = getGroupYScale(state, track.name);
    if (cfg.collapsed) {
      fixedChrome += track.element.offsetHeight || 34;
    } else {
      expanded.push(track);
      fixedChrome += 6; // 1px row border + 5px track resizer
    }
  }

  if (expanded.length === 0) return;
  const availH = Math.max(0, scrollH - fixedChrome);
  const nominalSum = expanded.reduce((sum, t) => sum + t.heightPx, 0);
  if (availH <= nominalSum || nominalSum <= 0) {
    for (const track of expanded) {
      track.renderedHeightPx = track.heightPx;
    }
    return;
  }

  let assigned = 0;
  for (let i = 0; i < expanded.length; i++) {
    const track = expanded[i];
    if (i === expanded.length - 1) {
      track.renderedHeightPx = Math.max(track.heightPx, availH - assigned);
    } else {
      const h = Math.max(track.heightPx, Math.floor((availH * track.heightPx) / nominalSum));
      track.renderedHeightPx = h;
      assigned += h;
    }
  }
}

export function resizeAllTracks(state: TimelineState): void {
  invalidateAxisSizeCache();
  invalidateOverviewSizeCache();
  computeTrackViewportHeights(state);
  for (const track of state.tracks) {
    const cfg = getGroupYScale(state, track.name);
    if (cfg.collapsed) continue;
    const width = Math.max(200, track.canvasWrap.clientWidth || 600);
    const height = track.renderedHeightPx;
    if (track.plot.width !== width || track.plot.height !== height) {
      track.plot.setSize({ width, height });
    } else {
      drawTrackOverlay(state, track);
    }
    track.overRect = null;
  }
  const axisCanvas = getSharedAxisCanvas();
  if (axisCanvas) {
    drawSharedAxis(axisCanvas, state);
  }
  drawOverview(state);
}

export function applyCachedWaveforms(state: TimelineState): void {
  if (state.lastColumns) {
    drawQueryResult(state, state.lastColumns.result);
  }
}

export function drawStreamFrame(state: TimelineState): void {
  const span = spanSeconds(state.division);
  const tEnd = state.viewEnd;
  const tStart = tEnd - span;
  const dpr = devicePixelRatio || 1;
  const bandAlpha = state.canvasTheme.isLight ? "24" : "33";

  for (const track of state.tracks) {
    if (getGroupYScale(state, track.name).collapsed) continue;
    const visibleFlags = track.members.map((m) => isSeriesVisible(state, m));
    const plotCssWidth = Math.max(
      200,
      Math.round(track.plot.bbox.width / dpr) || track.plot.width || 600,
    );
    const built = state.streamBuffer.buildAlignedTrackData(
      track.members,
      visibleFlags,
      tStart,
      tEnd,
      plotCssWidth,
      dpr,
      state.halts,
      track.hasBandSeries,
      (member) => colorOf(state, member),
      bandAlpha,
      track.name,
    );

    track.plot.batch(() => {
      for (let i = 0; i < built.seriesShow.length; i++) {
        const s = track.plot.series[1 + i];
        if (!s) continue;
        s.show = built.seriesShow[i];
        s.width = built.seriesWidth[i];
      }
      if (built.bandSig !== track.bandSig) {
        track.plot.delBand();
        for (const band of built.bands) {
          track.plot.addBand(band);
        }
        track.bandSig = built.bandSig;
      }
      track.plot.setData(built.data, false);
      track.plot.setScale("x", { min: tStart, max: tEnd });
      const yCfg = getGroupYScale(state, track.name);
      if (!yCfg.auto && yCfg.max > yCfg.min) {
        track.plot.setScale("y", { min: yCfg.min, max: yCfg.max });
      }
    });
  }

  syncHoverTimeFromRelX(state);
  const axisCanvas = getSharedAxisCanvas();
  if (axisCanvas) {
    drawSharedAxis(axisCanvas, state);
  }
  drawOverview(state);

  const tracksScroll = getTracksScroll();
  if (tracksScroll) {
    const rangeTag = `${Math.round(tStart * NS)},${Math.round(tEnd * NS)}`;
    if (tracksScroll.dataset.range !== rangeTag) {
      tracksScroll.dataset.range = rangeTag;
    }
  }
  updateLegendReadouts(state);
  updateCursorInspector(state);
}

export function drawQueryResult(
  state: TimelineState,
  result: QueryResult,
  queriedIds?: readonly number[],
  viewRangeOverride?: { startSec: number; endSec: number },
): void {
  if (result.unchanged) {
    return;
  }
  state.lastRevision = result.revision;

  const start = result.t0 / NS;
  const end = result.t1 / NS;
  const xMin = viewRangeOverride ? viewRangeOverride.startSec : start;
  const xMax = viewRangeOverride ? viewRangeOverride.endSec : end;
  if (start > 0 && (state.historyStart === 0 || start < state.historyStart)) {
    state.historyStart = start;
  }

  if (result.raw && result.rawXs) {
    const xs = result.rawXs;
    state.lastColumns = { result, xs };
    state.streamBuffer.seedFromRawResult(result, queriedIds);
    for (const track of state.tracks) {
      if (!track.hasBandSeries) {
        const aligned = new Array<Float64Array>(1 + track.members.length);
        aligned[0] = xs;
        for (let mIdx = 0; mIdx < track.members.length; mIdx++) {
          const member = track.members[mIdx];
          const data = result.series.get(member.seriesId);
          const visible = isSeriesVisible(state, member);
          aligned[1 + mIdx] =
            data && data.max.length === xs.length
              ? data.max
              : emptyNanFloat(track, mIdx, xs.length);
          const s = track.plot.series[1 + mIdx];
          if (s) {
            s.show = visible;
            s.width = 1.05;
          }
        }
        track.plot.batch(() => {
          if (track.bandSig !== "") {
            track.plot.delBand();
            track.bandSig = "";
          }
          track.plot.setData(aligned as unknown as uPlot.AlignedData, false);
          track.plot.setScale("x", { min: xMin, max: xMax });
          const yCfg = getGroupYScale(state, track.name);
          if (!yCfg.auto && yCfg.max > yCfg.min) {
            track.plot.setScale("y", { min: yCfg.min, max: yCfg.max });
          }
        });
      } else {
        const aligned = new Array<Float64Array | (number | null | undefined)[]>(
          1 + track.members.length * 2,
        );
        aligned[0] = xs;
        for (let mIdx = 0; mIdx < track.members.length; mIdx++) {
          const member = track.members[mIdx];
          const data = result.series.get(member.seriesId);
          const visible = isSeriesVisible(state, member);
          const maxSlot = mIdx * 2;
          const minSlot = maxSlot + 1;
          aligned[1 + maxSlot] =
            data && data.max.length === xs.length
              ? data.max
              : emptyUndefined(track, maxSlot, xs.length);
          aligned[1 + minSlot] = emptyUndefined(track, minSlot, xs.length);
          const sMax = track.plot.series[1 + maxSlot];
          const sMin = track.plot.series[1 + minSlot];
          if (sMax) {
            sMax.show = visible;
            sMax.width = 1.05;
          }
          if (sMin) sMin.show = false;
        }
        track.plot.batch(() => {
          if (track.bandSig !== "") {
            track.plot.delBand();
            track.bandSig = "";
          }
          track.plot.setData(aligned as unknown as uPlot.AlignedData, false);
          track.plot.setScale("x", { min: xMin, max: xMax });
          const yCfg = getGroupYScale(state, track.name);
          if (!yCfg.auto && yCfg.max > yCfg.min) {
            track.plot.setScale("y", { min: yCfg.min, max: yCfg.max });
          }
        });
      }
    }
  } else {
    state.streamBuffer.markSeeded(true);
    const columns = result.columns;
    const step = columns > 0 ? (result.t1 - result.t0) / columns / NS : 0;
    if (state.scratchXs.length !== columns) {
      state.scratchXs = new Float64Array(columns);
    }
    const xs = state.scratchXs;
    for (let i = 0; i < columns; i++) {
      xs[i] = start + i * step;
    }
    const gaps = populateGapMask(state, start, end, step, columns);
    state.lastColumns = { result, xs };

    const totalPts2 = columns * 2;
    const xs2 = getScratchQueryXs2(totalPts2);
    for (let i = 0; i < columns; i++) {
      xs2[i * 2] = start + (i + 0.25) * step;
      xs2[i * 2 + 1] = start + (i + 0.75) * step;
    }

    const bandAlpha = state.canvasTheme.isLight ? "24" : "33";
    for (const track of state.tracks) {
      if (!track.hasBandSeries) {
        const aligned = new Array<Float64Array>(1 + track.members.length);
        aligned[0] = xs2;
        for (let mIdx = 0; mIdx < track.members.length; mIdx++) {
          const member = track.members[mIdx];
          const visible = isSeriesVisible(state, member);
          const data = result.series.get(member.seriesId);
          const out = getScratchFloat(track, mIdx, totalPts2);
          if (data) {
            for (let i = 0; i < columns; i++) {
              out[i * 2] = data.min[i];
              out[i * 2 + 1] = data.max[i];
            }
          } else {
            out.fill(NaN);
          }
          aligned[1 + mIdx] = out;
          const s = track.plot.series[1 + mIdx];
          if (s) {
            s.show = visible;
            s.width = 1.05;
          }
        }
        track.plot.batch(() => {
          if (track.bandSig !== "") {
            track.plot.delBand();
            track.bandSig = "";
          }
          track.plot.setData(aligned as unknown as uPlot.AlignedData, false);
          track.plot.setScale("x", { min: xMin, max: xMax });
          const yCfg = getGroupYScale(state, track.name);
          if (!yCfg.auto && yCfg.max > yCfg.min) {
            track.plot.setScale("y", { min: yCfg.min, max: yCfg.max });
          }
        });
      } else {
        const yMinScale = track.plot.scales.y.min ?? -1;
        const yMaxScale = track.plot.scales.y.max ?? 1;
        const ySpan = Math.max(1e-9, yMaxScale - yMinScale);
        const spreadThreshold = (0.75 / Math.max(40, track.renderedHeightPx)) * ySpan;

        const aligned = new Array<Float64Array | (number | null | undefined)[]>(
          1 + track.members.length * 2,
        );
        aligned[0] = xs;
        const nextBands: uPlot.Band[] = [];
        const sigParts: string[] = [];

        for (let mIdx = 0; mIdx < track.members.length; mIdx++) {
          const member = track.members[mIdx];
          const visible = isSeriesVisible(state, member);
          const data = result.series.get(member.seriesId);
          const spread =
            visible && data ? hasMeaningfulSpread(data.min, data.max, spreadThreshold) : false;
          const maxSlot = mIdx * 2;
          const minSlot = maxSlot + 1;
          aligned[1 + maxSlot] = toSeriesArray(track, maxSlot, data?.max, columns, gaps);
          aligned[1 + minSlot] = spread
            ? toSeriesArray(track, minSlot, data?.min, columns, gaps)
            : emptyUndefined(track, minSlot, columns);

          const sMax = track.plot.series[1 + maxSlot];
          const sMin = track.plot.series[1 + minSlot];
          if (sMax) {
            sMax.show = visible;
            sMax.width = 1.05;
          }
          if (sMin) {
            sMin.show = visible && spread;
            sMin.width = 1;
          }
          if (visible && spread) {
            const color = colorOf(state, member);
            nextBands.push({
              series: [1 + maxSlot, 1 + minSlot],
              fill: withAlpha(color, bandAlpha),
            });
            sigParts.push(`${mIdx}:${color}`);
          }
        }
        const nextSig = sigParts.join(",");
        track.plot.batch(() => {
          if (nextSig !== track.bandSig) {
            track.plot.delBand();
            for (const band of nextBands) {
              track.plot.addBand(band);
            }
            track.bandSig = nextSig;
          }
          track.plot.setData(aligned as unknown as uPlot.AlignedData, false);
          track.plot.setScale("x", { min: xMin, max: xMax });
          const yCfg = getGroupYScale(state, track.name);
          if (!yCfg.auto && yCfg.max > yCfg.min) {
            track.plot.setScale("y", { min: yCfg.min, max: yCfg.max });
          }
        });
      }
    }
  }

  syncHoverTimeFromRelX(state);
  const axisCanvas = getSharedAxisCanvas();
  if (axisCanvas) {
    drawSharedAxis(axisCanvas, state);
  }
  drawOverview(state);

  const tracksScroll = getTracksScroll();
  if (tracksScroll) {
    const rangeTag = `${Math.round(xMin * NS)},${Math.round(xMax * NS)}`;
    if (tracksScroll.dataset.range !== rangeTag) {
      tracksScroll.dataset.range = rangeTag;
    }
  }
  updateLegendReadouts(state);
  updateCursorInspector(state);
}

export function rebuildTracks(
  state: TimelineState,
  legendCallbacks: LegendCallbacks,
  interactionCallbacks: InteractionCallbacks,
): void {
  ensureScrollRectSync(state);
  destroyTracks(state);

  if (state.soloSeriesId !== null && !state.series.some((s) => s.seriesId === state.soloSeriesId)) {
    state.soloSeriesId = null;
  }
  if (
    state.selectedSeriesId !== null &&
    !state.series.some((s) => s.seriesId === state.selectedSeriesId)
  ) {
    state.selectedSeriesId = state.series[0]?.seriesId ?? null;
  }

  const activeSeries = state.series.filter((item) => item.actualMode !== "none");
  const trackNames = [...new Set(activeSeries.map((item) => effectiveGroupOf(state, item)))];

  if (state.selectedTrack === null || !trackNames.includes(state.selectedTrack)) {
    state.selectedTrack = trackNames[0] ?? null;
  }

  const emptyEl = document.getElementById("empty");
  const tracksList = document.getElementById("tracks-list");
  const sharedAxisBar = document.getElementById("shared-axis-bar");
  if (emptyEl) emptyEl.hidden = state.series.length > 0;
  if (tracksList) tracksList.hidden = trackNames.length === 0;
  if (sharedAxisBar) sharedAxisBar.hidden = trackNames.length === 0;

  if (!tracksList) return;

  const seriesCountEl = document.getElementById("series-count");
  if (seriesCountEl) {
    seriesCountEl.textContent = String(state.series.length);
  }

  for (const name of trackNames) {
    const members = activeSeries.filter((item) => effectiveGroupOf(state, item) === name);
    const track = buildTrackRow(
      state,
      name,
      members,
      tracksList,
      legendCallbacks,
      interactionCallbacks,
    );
    state.tracks.push(track);
  }

  updateTrackSelectionHighlight(state);
  resizeAllTracks(state);

  const ids = state.tracks.flatMap((t) => t.members.map((m) => m.seriesId));
  const t0 = state.viewEnd - spanSeconds(state.division);
  if (ids.length > 0 && state.streamBuffer.canServe(ids, t0, state.historyStart)) {
    drawStreamFrame(state);
  } else if (state.lastColumns) {
    drawQueryResult(state, state.lastColumns.result);
  } else {
    updateLegendReadouts(state);
    updateCursorInspector(state);
    const axisCanvas = getSharedAxisCanvas();
    if (axisCanvas) drawSharedAxis(axisCanvas, state);
  }
}

function buildTrackRow(
  state: TimelineState,
  name: string,
  members: SeriesInfo[],
  container: HTMLElement,
  legendCallbacks: LegendCallbacks,
  interactionCallbacks: InteractionCallbacks,
): TrackRow {
  const yCfg = getGroupYScale(state, name);
  const heightPx = state.trackHeights.get(name) ?? DEFAULT_TRACK_HEIGHT;

  const element = document.createElement("div");
  element.className =
    "track-row" +
    (yCfg.collapsed ? " is-collapsed" : "") +
    (state.selectedTrack === name ? " is-selected" : "");
  element.dataset.track = name;

  element.addEventListener("dragover", (event) => {
    if (!state.dragging) return;
    event.preventDefault();
    if (event.dataTransfer) {
      event.dataTransfer.dropEffect = state.dragging.kind === "rtt" ? "copy" : "move";
    }
    element.classList.add("is-drop-target");
  });
  element.addEventListener("dragleave", (event) => {
    if (!element.contains(event.relatedTarget as Node | null)) {
      element.classList.remove("is-drop-target");
    }
  });
  element.addEventListener("drop", (event) => {
    event.preventDefault();
    element.classList.remove("is-drop-target");
    if (!state.dragging) return;
    if (state.dragging.kind === "series") {
      legendCallbacks.onMoveSeriesToTrack(state.dragging.seriesId, name);
    } else if (state.dragging.kind === "rtt") {
      legendCallbacks.onPlotRttSignal(state.dragging.signalName, name);
    }
    state.dragging = null;
    legendCallbacks.onDragStateChange(false);
  });

  const body = document.createElement("div");
  body.className = "track-body";

  const legendEl = document.createElement("div");
  legendEl.className = "track-legend";
  legendEl.addEventListener("click", () => {
    interactionCallbacks.onSelectTrack(name);
  });

  const firstBadge = members.length > 0 ? describeModeBadge(members[0]) : null;
  const isUniformMode =
    firstBadge !== null &&
    members.every((m) => !m.error && describeModeBadge(m).label === firstBadge.label);
  const uniformBadge = isUniformMode ? firstBadge : null;

  const headerRefs = renderTrackLegendHeader(
    legendEl,
    name,
    state,
    legendCallbacks,
    uniformBadge,
  );
  const signalRefs = renderTrackLegendSignals(
    legendEl,
    name,
    members,
    state,
    legendCallbacks,
    isUniformMode,
  );

  const plotEl = document.createElement("div");
  plotEl.className = "track-plot";

  const canvasWrap = document.createElement("div");
  canvasWrap.className = "track-canvas-wrap";
  plotEl.append(canvasWrap);

  body.append(legendEl, plotEl);

  const resizer = document.createElement("div");
  resizer.className = "track-resizer";
  resizer.title = "Drag to resize track height";

  element.append(body, resizer);
  container.append(element);

  const theme = state.canvasTheme;
  const hasBandSeries = state.showBands;
  const definitions: uPlot.Series[] = [{}];
  const bands: uPlot.Band[] = [];
  const bandSigParts: string[] = [];
  for (let mIdx = 0; mIdx < members.length; mIdx++) {
    const item = members[mIdx];
    const strokeColor = () => colorOf(state, item);
    const visible = isSeriesVisible(state, item);
    const base = {
      stroke: strokeColor,
      width: 1.05,
      show: visible,
      points: { show: false },
      spanGaps: false,
    };
    if (!hasBandSeries) {
      definitions.push({
        ...base,
        label: item.name,
        paths: nullPathBuilder,
        points: {
          show: () => state.division <= MARKERS_AT_DIVISION,
          size: 4.5,
          fill: strokeColor,
          stroke: strokeColor,
        },
      });
    } else {
      definitions.push(
        {
          ...base,
          label: `${item.name} max`,
          points: {
            show: () => state.division <= MARKERS_AT_DIVISION,
            size: 4.5,
            fill: strokeColor,
            stroke: strokeColor,
          },
        },
        { ...base, label: `${item.name} min`, width: 1 },
      );
      if (visible) {
        const color = colorOf(state, item);
        const idx = definitions.length - 2;
        bands.push({
          series: [idx, idx + 1],
          fill: withAlpha(color, theme.isLight ? "24" : "33"),
        });
        bandSigParts.push(`${mIdx}:${color}`);
      }
    }
  }

  const overlayCanvas = document.createElement("canvas");
  overlayCanvas.className = "track-overlay-canvas";

  let trackRef: TrackRow | null = null;
  const plotWidth = Math.max(200, canvasWrap.clientWidth || 600);
  const initialAligned = (
    hasBandSeries ? [[], ...members.flatMap(() => [[], []])] : [[], ...members.map(() => [])]
  ) as unknown as uPlot.AlignedData;
  const plot = new uPlot(
    {
      width: plotWidth,
      height: yCfg.collapsed ? 80 : heightPx,
      legend: { show: false },
      select: { show: false, left: 0, top: 0, width: 0, height: 0 },
      cursor: { show: false },
      padding: [6, 8, 2, 0],
      scales: {
        x: { time: false, auto: false },
        y: {
          auto: () => getGroupYScale(state, name).auto,
          range: (_u, dataMin, dataMax) => {
            const cfg = getGroupYScale(state, name);
            if (!cfg.auto && cfg.max > cfg.min) {
              return [cfg.min, cfg.max];
            }
            const lo = dataMin ?? -1;
            const hi = dataMax ?? 1;
            const next = stabilizeAutoYRange(trackRef?.autoYCache ?? null, lo, hi, (a, b) => {
              const [rMin, rMax] = uPlot.rangeNum(a, b, 0.08, true);
              return [rMin ?? a, rMax ?? b];
            });
            if (trackRef) {
              trackRef.autoYCache = next;
            }
            cfg.min = next.min;
            cfg.max = next.max;
            return [cfg.min, cfg.max];
          },
        },
      },
      axes: [
        {
          stroke: theme.axisStroke,
          grid: { stroke: theme.gridMajor },
          ticks: { stroke: theme.tickStroke, size: 0 },
          splits: (_u, _axis, min, max) => divisionSplits(state.division, min, max),
          values: () => [],
          size: 2,
        },
        {
          stroke: theme.axisStroke,
          grid: { stroke: theme.gridMinor },
          ticks: { stroke: theme.tickStroke, size: 4 },
          space: 52,
          values: (_u, splits) => splits.map((v) => formatAxisSi(v)),
          size: Y_AXIS_SIZE,
          font: "11px ui-monospace, monospace",
        },
      ],
      series: definitions,
      bands,
      hooks: {
        drawSeries: [
          (u, sIdx) => {
            if (!hasBandSeries) {
              drawDsoSeries(u, sIdx, state.halts);
            }
          },
        ],
        draw: [
          () => {
            if (trackRef) {
              drawTrackOverlay(state, trackRef);
            }
          },
        ],
      },
    },
    initialAligned,
    canvasWrap,
  );

  canvasWrap.append(overlayCanvas);

  const trackObj: TrackRow = {
    name,
    members,
    plot,
    overlayCanvas,
    overRect: null,
    element,
    legendEl,
    plotEl,
    canvasWrap,
    legendSignals: signalRefs.legendSignals,
    legendValues: signalRefs.legendValues,
    legendCards: signalRefs.legendCards,
    yAutoBtn: headerRefs.yAutoBtn,
    yRangeWrap: headerRefs.yRangeWrap,
    yMinInput: headerRefs.yMinInput,
    yMaxInput: headerRefs.yMaxInput,
    heightPx,
    renderedHeightPx: heightPx,
    hasBandSeries,
    scratchSeries: new Array<(number | null | undefined)[]>(members.length * 2),
    scratchFloatSeries: new Array<Float64Array>(members.length),
    bandSig: bandSigParts.join(","),
    autoYCache: null,
  };
  trackRef = trackObj;
  drawTrackOverlay(state, trackObj);

  const interactionAc = attachTrackInteractions(state, trackObj, interactionCallbacks);
  trackAbortControllers.push(interactionAc);

  const resizeAc = new AbortController();
  let resizing = false;
  let startY = 0;
  let startH = heightPx;

  resizer.addEventListener(
    "mousedown",
    (event) => {
      if (event.button !== 0 || getGroupYScale(state, name).collapsed) return;
      event.preventDefault();
      resizing = true;
      startY = event.clientY;
      startH = trackObj.renderedHeightPx;
      resizer.classList.add("dragging");
    },
    { signal: resizeAc.signal },
  );
  window.addEventListener(
    "mousemove",
    (event) => {
      if (!resizing) return;
      const nextH = Math.max(
        MIN_TRACK_HEIGHT,
        Math.min(MAX_TRACK_HEIGHT, Math.round(startH + (event.clientY - startY))),
      );
      if (nextH === trackObj.heightPx) return;
      trackObj.heightPx = nextH;
      state.trackHeights.set(name, nextH);
      resizeAllTracks(state);
    },
    { signal: resizeAc.signal },
  );
  window.addEventListener(
    "mouseup",
    () => {
      if (!resizing) return;
      resizing = false;
      resizer.classList.remove("dragging");
      saveTrackHeights(state.trackHeights);
    },
    { signal: resizeAc.signal },
  );
  trackAbortControllers.push(resizeAc);

  return trackObj;
}
