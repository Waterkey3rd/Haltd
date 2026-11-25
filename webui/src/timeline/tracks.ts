import uPlot from "uplot";
import type { QueryResult, SeriesInfo } from "../client";
import {
  drawSharedAxis,
  drawTrackOverlay,
  updateCursorInspector,
} from "./cursors";
import { attachTrackInteractions, type InteractionCallbacks } from "./interactions";
import {
  renderTrackLegendHeader,
  renderTrackLegendSignals,
  updateLegendReadouts,
  type LegendCallbacks,
} from "./legend";
import { divisionSplits, formatNumber, hasAnyNaN, MARKERS_AT_DIVISION } from "./math";
import { drawOverview } from "./overview";
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
  toHexColor,
  withAlpha,
  Y_AXIS_SIZE,
  type TimelineState,
  type TrackRow,
} from "./state";

let trackAbortControllers: AbortController[] = [];
let cachedTracksScroll: HTMLElement | null = null;
let cachedSharedAxisCanvas: HTMLCanvasElement | null = null;

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

function populateGapMask(
  state: TimelineState,
  start: number,
  end: number,
  step: number,
  columns: number,
): Uint8Array | null {
  if (columns <= 0 || state.halts.length === 0 || step <= 0) {
    return null;
  }
  let anyOverlap = false;
  for (let i = state.halts.length - 1; i >= 0; i--) {
    const halt = state.halts[i];
    const hEnd = halt.to ?? Infinity;
    if (hEnd < start) {
      break;
    }
    if (halt.from <= end) {
      anyOverlap = true;
      break;
    }
  }
  if (!anyOverlap) {
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
    track.plot.batch(() => {
      track.plot.delBand();
      for (let mIdx = 0; mIdx < track.members.length; mIdx++) {
        const item = track.members[mIdx];
        const visible = isSeriesVisible(state, item);
        const color = colorOf(state, item);
        const maxIdx = 1 + mIdx * 2;
        const minIdx = maxIdx + 1;

        const dom = track.legendSignals.get(item.seriesId);
        if (dom) {
          dom.row.classList.toggle("is-hidden", !visible);
          dom.checkbox.checked = visible;
          dom.checkbox.style.accentColor = color;
          dom.swatch.value = toHexColor(color);
          dom.soloBtn.classList.toggle("active", state.soloSeriesId === item.seriesId);
        }

        const sMax = track.plot.series[maxIdx];
        const sMin = track.plot.series[minIdx];
        if (sMax) {
          sMax.show = visible;
        }
        if (sMin) {
          sMin.show = visible;
          sMin.width = state.showBands ? 1 : 1.5;
        }
        if (state.showBands && visible) {
          track.plot.addBand({
            series: [maxIdx, minIdx],
            fill: withAlpha(color, bandAlpha),
          });
        }
      }
      track.plot.redraw(true);
    });
  }
  updateLegendReadouts(state);
  updateCursorInspector(state);
}

/**
 * Redraws only lightweight interactive overlay canvases (crosshair, cursors, pins,
 * shared axis, and overview) without repainting uPlot waveform paths or grids.
 */
export function redrawAllOverlays(state: TimelineState): void {
  for (const track of state.tracks) {
    if (getGroupYScale(state, track.name).collapsed) continue;
    drawTrackOverlay(state, track);
  }
  const axisCanvas = getSharedAxisCanvas();
  if (axisCanvas) {
    drawSharedAxis(axisCanvas, state);
  }
  drawOverview(state);
}

export function resizeAllTracks(state: TimelineState): void {
  for (const track of state.tracks) {
    const cfg = getGroupYScale(state, track.name);
    if (cfg.collapsed) continue;
    const width = Math.max(200, track.canvasWrap.clientWidth || 600);
    if (track.plot.width !== width || track.plot.height !== track.heightPx) {
      track.plot.setSize({ width, height: track.heightPx });
    } else {
      drawTrackOverlay(state, track);
    }
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

export function drawQueryResult(state: TimelineState, result: QueryResult): void {
  const columns = result.columns;
  const step = columns > 0 ? (result.t1 - result.t0) / columns / NS : 0;
  const start = result.t0 / NS;
  const end = result.t1 / NS;
  if (start > 0 && (state.historyStart === 0 || start < state.historyStart)) {
    state.historyStart = start;
  }

  if (state.scratchXs.length !== columns) {
    state.scratchXs = new Float64Array(columns);
  }
  const xs = state.scratchXs;
  for (let i = 0; i < columns; i++) {
    xs[i] = start + i * step;
  }
  const gaps = populateGapMask(state, start, end, step, columns);
  state.lastColumns = { result, xs };

  const tracksScroll = getTracksScroll();
  if (tracksScroll) {
    const rangeTag = `${result.t0},${result.t1}`;
    if (tracksScroll.dataset.range !== rangeTag) {
      tracksScroll.dataset.range = rangeTag;
    }
  }

  for (const track of state.tracks) {
    const aligned = new Array<Float64Array | (number | null | undefined)[]>(
      1 + track.members.length * 2,
    );
    aligned[0] = xs;
    for (let mIdx = 0; mIdx < track.members.length; mIdx++) {
      const member = track.members[mIdx];
      const data = result.series.get(member.seriesId);
      const maxSlot = mIdx * 2;
      const minSlot = maxSlot + 1;
      aligned[1 + maxSlot] = toSeriesArray(track, maxSlot, data?.max, columns, gaps);
      aligned[1 + minSlot] = toSeriesArray(track, minSlot, data?.min, columns, gaps);
    }
    track.plot.batch(() => {
      track.plot.setData(aligned as unknown as uPlot.AlignedData, false);
      track.plot.setScale("x", { min: start, max: end });
      const yCfg = getGroupYScale(state, track.name);
      if (!yCfg.auto && yCfg.max > yCfg.min) {
        track.plot.setScale("y", { min: yCfg.min, max: yCfg.max });
      }
    });
  }

  updateLegendReadouts(state);
  updateCursorInspector(state);
  const axisCanvas = getSharedAxisCanvas();
  if (axisCanvas) {
    drawSharedAxis(axisCanvas, state);
  }
  drawOverview(state);
}

export function rebuildTracks(
  state: TimelineState,
  legendCallbacks: LegendCallbacks,
  interactionCallbacks: InteractionCallbacks,
): void {
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

  if (state.lastColumns) {
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

  const headerRefs = renderTrackLegendHeader(legendEl, name, state, legendCallbacks);
  const signalRefs = renderTrackLegendSignals(legendEl, name, members, state, legendCallbacks);

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
  const definitions: uPlot.Series[] = [{}];
  const bands: uPlot.Band[] = [];
  for (const item of members) {
    const strokeColor = () => colorOf(state, item);
    const visible = isSeriesVisible(state, item);
    const base = {
      stroke: strokeColor,
      width: 1.5,
      show: visible,
      points: { show: false },
      spanGaps: false,
    };
    definitions.push(
      {
        ...base,
        label: `${item.name} max`,
        points: {
          show: () => state.division <= MARKERS_AT_DIVISION,
          size: 5,
          fill: strokeColor,
          stroke: strokeColor,
        },
      },
      { ...base, label: `${item.name} min`, width: state.showBands ? 1 : 1.5 },
    );
    if (state.showBands && visible) {
      const idx = definitions.length - 2;
      bands.push({
        series: [idx, idx + 1],
        fill: withAlpha(colorOf(state, item), theme.isLight ? "24" : "33"),
      });
    }
  }

  const overlayCanvas = document.createElement("canvas");
  overlayCanvas.className = "track-overlay-canvas";

  let trackRef: TrackRow | null = null;
  const plotWidth = Math.max(200, canvasWrap.clientWidth || 600);
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
            const [rMin, rMax] = uPlot.rangeNum(lo, hi, 0.08, true);
            cfg.min = rMin ?? lo;
            cfg.max = rMax ?? hi;
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
          ticks: { stroke: theme.tickStroke },
          size: Y_AXIS_SIZE,
          font: "11px ui-monospace, monospace",
        },
      ],
      series: definitions,
      bands,
      hooks: {
        draw: [
          () => {
            if (trackRef) {
              drawTrackOverlay(state, trackRef);
            }
          },
        ],
      },
    },
    [[], ...members.flatMap(() => [[], []])] as unknown as uPlot.AlignedData,
    canvasWrap,
  );

  canvasWrap.append(overlayCanvas);

  const trackObj: TrackRow = {
    name,
    members,
    plot,
    overlayCanvas,
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
    scratchSeries: new Array<(number | null | undefined)[]>(members.length * 2),
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
      startH = trackObj.heightPx;
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
      const width = Math.max(200, trackObj.canvasWrap.clientWidth || 600);
      trackObj.plot.setSize({ width, height: nextH });
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
