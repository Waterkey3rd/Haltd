import uPlot from "uplot";
import type { QueryResult, SeriesInfo } from "../client";
import {
  drawOverlays,
  drawSharedAxis,
  updateCursorInspector,
} from "./cursors";
import { attachTrackInteractions, type InteractionCallbacks } from "./interactions";
import {
  renderTrackLegendHeader,
  renderTrackLegendSignals,
  updateLegendReadouts,
  type LegendCallbacks,
} from "./legend";
import { divisionSplits, formatNumber, MARKERS_AT_DIVISION } from "./math";
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
  withAlpha,
  Y_AXIS_SIZE,
  type TimelineState,
  type TrackRow,
} from "./state";

let trackAbortControllers: AbortController[] = [];

export function isHalted(state: TimelineState, seconds: number): boolean {
  for (let index = state.halts.length - 1; index >= 0; index--) {
    const halt = state.halts[index];
    if (seconds >= halt.from && seconds <= (halt.to ?? Infinity)) return true;
    if (halt.to !== null && halt.to < seconds) return false;
  }
  return false;
}

function toPlotValues(data: Float64Array, gaps: boolean[]): (number | null | undefined)[] {
  const out = new Array<number | null | undefined>(data.length);
  for (let index = 0; index < data.length; index++) {
    const value = data[index];
    out[index] = Number.isNaN(value) ? (gaps[index] ? null : undefined) : value;
  }
  return out;
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

export function redrawAllOverlays(state: TimelineState): void {
  for (const track of state.tracks) {
    track.plot.redraw(false);
  }
  const axisCanvas = document.getElementById("shared-axis-canvas") as HTMLCanvasElement | null;
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
    track.plot.setSize({ width, height: track.heightPx });
  }
  const axisCanvas = document.getElementById("shared-axis-canvas") as HTMLCanvasElement | null;
  if (axisCanvas) {
    drawSharedAxis(axisCanvas, state);
  }
  drawOverview(state);
}

export function drawQueryResult(state: TimelineState, result: QueryResult): void {
  const step = (result.t1 - result.t0) / result.columns / NS;
  const start = result.t0 / NS;
  const end = result.t1 / NS;
  if (start > 0 && (state.historyStart === 0 || start < state.historyStart)) {
    state.historyStart = start;
  }
  const xs = Array.from({ length: result.columns }, (_, index) => start + index * step);
  const gaps = xs.map((t) => isHalted(state, t));
  state.lastColumns = { result, xs };

  const tracksScroll = document.getElementById("tracks-scroll");
  if (tracksScroll) {
    tracksScroll.dataset.range = `${result.t0},${result.t1}`;
  }

  for (const track of state.tracks) {
    const arrays: (number | null | undefined)[][] = [];
    for (const member of track.members) {
      const data = result.series.get(member.seriesId);
      if (data) {
        arrays.push(toPlotValues(data.max, gaps), toPlotValues(data.min, gaps));
      } else {
        const empty = new Array<null>(result.columns).fill(null);
        arrays.push(empty, empty);
      }
    }
    track.plot.batch(() => {
      track.plot.setData([xs, ...arrays] as unknown as uPlot.AlignedData, false);
      track.plot.setScale("x", { min: start, max: end });
      const yCfg = getGroupYScale(state, track.name);
      if (!yCfg.auto && yCfg.max > yCfg.min) {
        track.plot.setScale("y", { min: yCfg.min, max: yCfg.max });
      }
    });
  }

  updateLegendReadouts(state);
  updateCursorInspector(state);
  const axisCanvas = document.getElementById("shared-axis-canvas") as HTMLCanvasElement | null;
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
    const axisCanvas = document.getElementById("shared-axis-canvas") as HTMLCanvasElement | null;
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

  // Drag & Drop target for moving series or adding RTT signals onto this track
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
    const color = colorOf(state, item, state.series.indexOf(item));
    const visible = isSeriesVisible(state, item);
    const base = {
      stroke: color,
      width: 1.5,
      show: visible,
      points: { show: false },
      spanGaps: false,
    } as const;
    definitions.push(
      {
        ...base,
        label: `${item.name} max`,
        points: {
          show: () => state.division <= MARKERS_AT_DIVISION,
          size: 5,
          fill: color,
          stroke: color,
        },
      },
      { ...base, label: `${item.name} min`, width: state.showBands ? 1 : 1.5 },
    );
    if (state.showBands && visible) {
      const idx = definitions.length - 2;
      bands.push({
        series: [idx, idx + 1],
        fill: withAlpha(color, theme.isLight ? "24" : "33"),
      });
    }
  }

  const plotWidth = Math.max(200, canvasWrap.clientWidth || 600);
  const plot = new uPlot(
    {
      width: plotWidth,
      height: yCfg.collapsed ? 80 : heightPx,
      legend: { show: false },
      padding: [6, 8, 2, 0],
      cursor: {
        drag: { x: false, y: false, setScale: false },
        sync: { key: "timeline" },
      },
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
      hooks: { draw: [drawOverlays(state, name, members)] },
    },
    [[], ...members.flatMap(() => [[], []])] as unknown as uPlot.AlignedData,
    canvasWrap,
  );

  const trackObj: TrackRow = {
    name,
    members,
    plot,
    element,
    legendEl,
    plotEl,
    canvasWrap,
    legendValues: signalRefs.legendValues,
    legendCards: signalRefs.legendCards,
    yAutoBtn: headerRefs.yAutoBtn,
    yRangeWrap: headerRefs.yRangeWrap,
    yMinInput: headerRefs.yMinInput,
    yMaxInput: headerRefs.yMaxInput,
    heightPx,
  };

  const interactionAc = attachTrackInteractions(state, trackObj, interactionCallbacks);
  trackAbortControllers.push(interactionAc);

  // Track height drag resizer
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
