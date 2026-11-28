import "uplot/dist/uPlot.min.css";
import "../style.css";
import {
  Client,
  EVENT_FAULT,
  EVENT_HALTED,
  EVENT_RELOAD,
  EVENT_RESET,
  EVENT_RESUMED,
  type QueryResult,
  type SeriesInfo,
  type TimelineEvent,
} from "../client";
import { ThemeManager } from "../theme";
import {
  initContextMenuDismiss,
  showContextMenu,
  type ContextMenuActions,
} from "./contextMenu";
import {
  attachSharedAxisInteractions,
  updateCursorInspector,
} from "./cursors";
import { ExportUiController } from "./exportCsv";
import { isInputElement, resolveShortcut } from "./keymap";
import { renderRttSignalsDrawer, updateLegendReadouts, type LegendCallbacks } from "./legend";
import {
  computeQueryLod,
  fitTimeRange,
  FOLLOW_LAG_S,
  spanSeconds,
  stepDivision,
} from "./math";
import { attachOverviewScrubber, drawOverview } from "./overview";
import { PresetController } from "./presets";
import { RenderScheduler } from "./scheduler";
import {
  addPin,
  clearAllPins,
  createInitialState,
  effectiveGroupOf,
  getGroupYScale,
  nextCustomTrackName,
  nowSeconds,
  removePin,
  setSeriesList,
  type RttSignalInfo,
} from "./state";
import {
  setConnectionStatus,
  showRate,
  stopFollowing,
  syncDivisionControls,
  syncFollowButton,
  syncRecordButton,
  updateEventsUi,
  updateSuggestions,
  wireToolbar,
  type RateReply,
} from "./toolbar";
import {
  applyCachedWaveforms,
  applySeriesFilter,
  applyTrackYScale,
  drawQueryResult,
  drawStreamFrame,
  hasHaltsInRange,
  rebuildTracks,
  redrawAllOverlays,
  redrawTrackOverlaysOnly,
  resizeAllTracks,
  syncTrackYControls,
  syncTracksAppearance,
  updateTrackSelectionHighlight,
} from "./tracks";

const STATUS_MS = 1000;
const NS = 1e9;

const themeManager = new ThemeManager();
const state = createInitialState(themeManager.getCanvasTheme());
const client = new Client();

let exportUi: ExportUiController | null = null;
let presetController: PresetController | null = null;

function setMeasureText(text: string): void {
  const m = document.getElementById("measure");
  if (m) m.textContent = text;
}

let lastStreamRenderEnd = -Infinity;
let lastStreamDivision = 0;

async function loadSeries(): Promise<void> {
  lastStreamRenderEnd = -Infinity;
  const reply = await client.call<{ series: SeriesInfo[] }>("timeline.list");
  setSeriesList(state, reply.series);
  const ids = state.series
    .filter((s) => s.actualMode !== "none")
    .map((s) => s.seriesId);
  state.streamBuffer.retainSeries(ids);
  void client
    .call("timeline.subscribe", { seriesIds: ids, replace: true })
    .catch(() => undefined);
  try {
    const rttReply = await client.call<{ signals: RttSignalInfo[] }>("rtt.signals");
    state.rttSignals = rttReply.signals ?? [];
    renderRttSignalsDrawer(state, legendCallbacks);
  } catch {
    // RTT signals optional
  }
  rebuild();
  if (ids.length > 0) {
    scheduler.invalidate("query");
  }
}

function hasLiveSources(): boolean {
  return (
    state.tracks.length > 0 &&
    state.series.some((s) => s.actualMode !== "off" && s.actualMode !== "none")
  );
}

async function executeQuery(): Promise<void> {
  state.querying = true;
  try {
    const ids = state.tracks.flatMap((t) => t.members.map((m) => m.seriesId));
    const plotCssWidth = state.tracks[0]?.plot.bbox.width
      ? Math.round(state.tracks[0].plot.bbox.width / (devicePixelRatio || 1))
      : 0;
    if (ids.length > 0 && plotCssWidth > 0) {
      const span = spanSeconds(state.division);
      const rawEnd = state.followLive ? nowSeconds(state) - FOLLOW_LAG_S : state.viewEnd;
      if (state.streamBuffer.canServe(ids, rawEnd - span, state.historyStart)) {
        if (state.followLive) state.viewEnd = rawEnd;
        lastStreamRenderEnd = state.viewEnd;
        lastStreamDivision = state.division;
        drawStreamFrame(state);
        return;
      }

      const inMotion = performance.now() < state.interactingUntil;
      const maxRateHz = state.series.reduce(
        (max, s) => Math.max(max, s.actualMode === "rtt" ? 1000 : (s.rateHz || 0)),
        0,
      );
      const hasHalts = hasHaltsInRange(state, rawEnd - span, rawEnd);
      const lod = computeQueryLod({
        plotCssWidth,
        spanSec: span,
        maxRateHz,
        inMotion,
        hasHaltsInWindow: hasHalts,
        dpr: devicePixelRatio || 1,
      });
      // Prefix-backfill StreamBuffer raw history up to 2s/div (22k pts) on demand.
      const expected = maxRateHz > 0 ? Math.ceil(span * maxRateHz) : 0;
      const maxRawPoints =
        !hasHalts && expected > 0 && expected <= 22_000
          ? Math.min(24_000, expected + 128)
          : lod.maxRawPoints;

      const spanNs = Math.round(span * NS);
      let t1Ns: number;
      let t0Ns: number;
      let queryWidthPx = lod.widthPx;
      let viewRangeOverride: { startSec: number; endSec: number } | undefined;
      if (state.followLive && maxRawPoints === 0) {
        state.viewEnd = rawEnd;
        const stepNs = Math.max(1, Math.round(spanNs / lod.widthPx));
        queryWidthPx = Math.min(8192, lod.widthPx + 2);
        t0Ns = Math.floor(Math.round((rawEnd - span) * NS) / stepNs) * stepNs;
        t1Ns = t0Ns + queryWidthPx * stepNs;
        viewRangeOverride = { startSec: state.viewEnd - span, endSec: state.viewEnd };
      } else {
        if (state.followLive) state.viewEnd = rawEnd;
        t1Ns = Math.round(rawEnd * NS);
        t0Ns = Math.round((rawEnd - span) * NS);
      }
      const followKey =
        state.followLive && maxRawPoints === 0 ? `${t1Ns}:${spanNs}:${queryWidthPx}` : "";
      const canShortCircuit =
        followKey !== "" && followKey === state.lastFollowQueryKey && state.lastRevision > 0;
      state.lastFollowQueryKey = followKey;

      const result = await client.call<QueryResult>("timeline.query", {
        seriesIds: ids,
        t0: t0Ns,
        t1: t1Ns,
        widthPx: queryWidthPx,
        ...(maxRawPoints > 0 ? { maxRawPoints } : {}),
        ...(canShortCircuit ? { ifChangedSince: state.lastRevision } : {}),
      });
      drawQueryResult(state, result, ids, viewRangeOverride);
    } else {
      drawOverview(state);
    }
  } catch {
    // Reconnecting; next wake/tick retries.
  } finally {
    state.querying = false;
  }
}

const scheduler = new RenderScheduler({
  isHidden: () => document.hidden,
  shouldFollowLive: () => state.followLive && hasLiveSources(),
  advanceLiveClock: (frameTimeMs = performance.now()) => {
    const hasFastHss = state.series.some(
      (s) => s.actualMode === "hss" && (s.rateHz || 0) >= 200,
    );
    state.viewEnd = state.clockPll.step(
      nowSeconds(state),
      frameTimeMs,
      state.streamBuffer.latestTimeSec,
      hasFastHss,
    );
  },
  renderLiveStream: () => {
    if (state.tracks.length === 0) return false;
    const ids = state.tracks.flatMap((t) => t.members.map((m) => m.seriesId));
    const span = spanSeconds(state.division);
    const t0 = state.viewEnd - span;
    if (!state.streamBuffer.canServe(ids, t0, state.historyStart)) {
      return false;
    }
    const plotCssWidth = Math.max(
      200,
      Math.round(
        (state.tracks[0]?.plot.bbox.width ?? 0) / (devicePixelRatio || 1),
      ) || state.tracks[0]?.plot.width || 1000,
    );
    const pixelDtSec = span / plotCssWidth;
    const movedEnough =
      state.division !== lastStreamDivision ||
      Math.abs(state.viewEnd - lastStreamRenderEnd) >= pixelDtSec * 0.45;
    if (movedEnough) {
      lastStreamRenderEnd = state.viewEnd;
      lastStreamDivision = state.division;
      drawStreamFrame(state);
    } else {
      updateLegendReadouts(state);
    }
    return true;
  },
  runQuery: () => executeQuery(),
  applyLayout: () => resizeAllTracks(state),
  redrawWaveforms: () => applyCachedWaveforms(state),
  redrawOverlays: () => redrawAllOverlays(state),
  updateReadouts: () => updateLegendReadouts(state),
  updateInspector: () => updateCursorInspector(state),
  getMinLiveIntervalMs: () => {
    const ids = state.tracks.flatMap((t) => t.members.map((m) => m.seriesId));
    const span = spanSeconds(state.division);
    const t0 = state.viewEnd - span;
    if (ids.length > 0 && state.streamBuffer.canServe(ids, t0, state.historyStart)) {
      return 0;
    }
    const plotCssWidth = Math.max(
      200,
      Math.round(
        (state.tracks[0]?.plot.bbox.width ?? 0) / (devicePixelRatio || 1),
      ) || state.tracks[0]?.plot.width || 1000,
    );
    const pixelDtSec = span / plotCssWidth;
    return Math.max(16, Math.min(45, Math.round(pixelDtSec * 450)));
  },
});

let settleTimer: number | undefined;
function markMotion(): void {
  state.interactingUntil = performance.now() + 140;
  if (settleTimer !== undefined) {
    window.clearTimeout(settleTimer);
  }
  settleTimer = window.setTimeout(() => {
    settleTimer = undefined;
    if (!state.followLive) {
      scheduler.invalidate("query");
    }
  }, 150);
}

function requestRefresh(): void {
  lastStreamRenderEnd = -Infinity;
  markMotion();
  scheduler.invalidate("query", "overlays");
}

async function refresh(): Promise<void> {
  lastStreamRenderEnd = -Infinity;
  markMotion();
  const ids = state.tracks.flatMap((t) => t.members.map((m) => m.seriesId));
  const span = spanSeconds(state.division);
  const end = state.followLive ? nowSeconds(state) - FOLLOW_LAG_S : state.viewEnd;
  if (ids.length > 0 && state.streamBuffer.canServe(ids, end - span, state.historyStart)) {
    if (state.followLive) {
      state.viewEnd = end;
      state.clockPll.reset();
    }
    lastStreamRenderEnd = state.viewEnd;
    lastStreamDivision = state.division;
    drawStreamFrame(state);
    scheduler.wake();
    return;
  }
  if (!scheduler.isQueryInFlight()) {
    await executeQuery();
    scheduler.wake();
  } else {
    scheduler.invalidate("query", "overlays");
  }
}

function zoomToRange(tA: number, tB: number): void {
  const fitted = fitTimeRange(tA, tB);
  stopFollowing(state);
  state.division = fitted.division;
  state.viewEnd = fitted.viewEnd;
  syncDivisionControls(state);
  requestRefresh();
}

function fitAllHistory(): void {
  const now = Math.max(nowSeconds(state) - FOLLOW_LAG_S, state.viewEnd, 0.5);
  const lo = Math.max(0, state.historyStart);
  zoomToRange(lo, Math.max(lo + 0.01, now));
}

function clearCursors(): void {
  state.cursorA = null;
  state.cursorB = null;
  state.sampleCursor = null;
  state.sampleCursorPinned = false;
  scheduler.invalidate("readouts", "inspector", "overlays");
}

function autoScaleTrackY(trackName: string): void {
  const track = state.tracks.find((t) => t.name === trackName);
  if (!track) return;
  const cfg = getGroupYScale(state, trackName);
  cfg.auto = true;
  track.autoYCache = null;
  syncTrackYControls(state, track);
  if (state.lastColumns) {
    scheduler.invalidate("waveforms");
  } else {
    requestRefresh();
  }
}

async function moveSeriesToTrack(seriesId: number, targetTrack: string | null): Promise<void> {
  const item = state.series.find((s) => s.seriesId === seriesId);
  if (!item) return;
  const nextGroup = targetTrack ?? nextCustomTrackName(state, item.name);
  item.group = nextGroup;
  if (state.splitLanes) {
    state.splitLanes = false;
    const btn = document.getElementById("layout-mode");
    if (btn) {
      btn.textContent = "Grouped";
      btn.setAttribute("aria-pressed", "false");
    }
  }
  state.selectedTrack = nextGroup;
  try {
    await client.call("timeline.update", {
      seriesId,
      group: nextGroup,
      ...(item.color ? { color: item.color } : {}),
    });
  } catch {
    // Keep local group fallback if series was removed concurrently
  }
  rebuild();
}

async function plotRttSignalToTrack(signalName: string, targetTrack: string | null): Promise<void> {
  const group = targetTrack ?? nextCustomTrackName(state, `rtt:${signalName}`);
  await client.call("timeline.add", {
    rttSignal: signalName,
    group,
  });
  state.selectedTrack = group;
  await loadSeries();
}

function toggleSignalHide(item: SeriesInfo): void {
  state.soloSeriesId = null;
  if (state.hidden.has(item.seriesId)) {
    state.hidden.delete(item.seriesId);
  } else {
    state.hidden.add(item.seriesId);
  }
  syncTracksAppearance(state);
  presetController?.syncDirtyIndicator();
}

function toggleSignalSolo(item: SeriesInfo): void {
  state.soloSeriesId = state.soloSeriesId === item.seriesId ? null : item.seriesId;
  syncTracksAppearance(state);
  presetController?.syncDirtyIndicator();
}

async function removeSignal(item: SeriesInfo): Promise<void> {
  if (state.soloSeriesId === item.seriesId) state.soloSeriesId = null;
  if (state.selectedSeriesId === item.seriesId) state.selectedSeriesId = null;
  await client.call("timeline.remove", { seriesId: item.seriesId });
  await loadSeries();
}

function pickColorForSignal(item: SeriesInfo): void {
  const picker = document.createElement("input");
  picker.type = "color";
  picker.style.position = "fixed";
  picker.style.opacity = "0";
  picker.style.pointerEvents = "none";
  document.body.append(picker);
  picker.onchange = async () => {
    item.color = picker.value;
    try {
      await client.call("timeline.update", {
        seriesId: item.seriesId,
        color: picker.value,
        ...(item.group ? { group: item.group } : {}),
      });
    } catch {
      // Ignore
    }
    picker.remove();
    syncTracksAppearance(state);
    presetController?.syncDirtyIndicator();
  };
  picker.onblur = () => picker.remove();
  picker.click();
}

const contextMenuActions: ContextMenuActions = {
  zoomToFit: () => fitAllHistory(),
  autoScaleTrackY: (trackName) => autoScaleTrackY(trackName),
  placeCursorA: (time) => {
    state.cursorA = time;
    scheduler.invalidate("readouts", "inspector", "overlays");
  },
  placeCursorB: (time) => {
    state.cursorB = time;
    scheduler.invalidate("readouts", "inspector", "overlays");
  },
  placeSampleCursor: (time) => {
    state.sampleCursor = time;
    state.sampleCursorPinned = true;
    scheduler.invalidate("readouts", "inspector", "overlays");
  },
  addPin: (time) => {
    addPin(state, time);
    scheduler.invalidate("readouts", "inspector", "overlays");
  },
  removePin: (id) => {
    if (removePin(state, id)) {
      scheduler.invalidate("readouts", "inspector", "overlays");
    }
  },
  clearPins: () => {
    clearAllPins(state);
    scheduler.invalidate("readouts", "inspector", "overlays");
  },
  clearCursors: () => clearCursors(),
  changeSignalColor: (item) => pickColorForSignal(item),
  moveSignalToTrack: (item, targetTrack) => void moveSeriesToTrack(item.seriesId, targetTrack),
  toggleHideSignal: (item) => toggleSignalHide(item),
  toggleSoloSignal: (item) => toggleSignalSolo(item),
  removeSignal: (item) => void removeSignal(item),
  exportVisibleCsv: () => exportUi?.openPopover(),
};

const legendCallbacks: LegendCallbacks = {
  onSelectSignal: (item, trackName) => {
    state.selectedSeriesId = item.seriesId;
    state.selectedTrack = trackName;
    updateTrackSelectionHighlight(state);
  },
  onToggleVisible: (item) => toggleSignalHide(item),
  onChangeColor: async (item, color) => {
    item.color = color;
    try {
      await client.call("timeline.update", {
        seriesId: item.seriesId,
        color,
        ...(item.group ? { group: item.group } : {}),
      });
    } catch {
      // Ignore
    }
    syncTracksAppearance(state);
    presetController?.syncDirtyIndicator();
  },
  onToggleSolo: (item) => toggleSignalSolo(item),
  onRemoveSignal: (item) => void removeSignal(item),
  onToggleAutoY: (trackName) => {
    const track = state.tracks.find((t) => t.name === trackName);
    if (!track) return;
    const cfg = getGroupYScale(state, trackName);
    if (cfg.auto) {
      cfg.auto = false;
      cfg.min = track.plot.scales.y.min ?? -1;
      cfg.max = track.plot.scales.y.max ?? 1;
    } else {
      cfg.auto = true;
      track.autoYCache = null;
    }
    syncTrackYControls(state, track);
    if (state.lastColumns) {
      scheduler.invalidate("waveforms");
    }
    presetController?.syncDirtyIndicator();
  },
  onManualYRange: (trackName, min, max) => {
    const track = state.tracks.find((t) => t.name === trackName);
    if (!track) return;
    const cfg = getGroupYScale(state, trackName);
    cfg.auto = false;
    cfg.min = min;
    cfg.max = max;
    applyTrackYScale(state, track);
    presetController?.syncDirtyIndicator();
  },
  onToggleCollapse: (trackName) => {
    const cfg = getGroupYScale(state, trackName);
    cfg.collapsed = !cfg.collapsed;
    rebuild();
  },
  onMoveSeriesToTrack: (seriesId, targetTrack) => void moveSeriesToTrack(seriesId, targetTrack),
  onPlotRttSignal: (signalName, targetTrack) => void plotRttSignalToTrack(signalName, targetTrack),
  onRemoveRttSeries: (seriesId) => {
    void client.call("timeline.remove", { seriesId }).then(() => loadSeries());
  },
  onContextMenuSignal: (event, trackName, signal) => {
    showContextMenu(
      state,
      {
        clientX: event.clientX,
        clientY: event.clientY,
        trackName,
        timeAtCursor: state.hoverTime ?? state.sampleCursor,
        signal,
      },
      contextMenuActions,
    );
  },
  onDragStateChange: (active) => {
    const dropZone = document.getElementById("new-track-drop");
    if (dropZone) dropZone.classList.toggle("is-active", active);
  },
};

const interactionCallbacks = {
  onSelectTrack: (trackName: string) => {
    state.selectedTrack = trackName;
    const track = state.tracks.find((t) => t.name === trackName);
    if (
      track &&
      (state.selectedSeriesId === null ||
        !track.members.some((m) => m.seriesId === state.selectedSeriesId))
    ) {
      state.selectedSeriesId = track.members[0]?.seriesId ?? null;
    }
    updateTrackSelectionHighlight(state);
  },
  onStopFollowing: () => stopFollowing(state),
  onRefresh: () => {
    syncDivisionControls(state);
    requestRefresh();
  },
  onRedrawOverlays: () => scheduler.invalidate("overlays"),
  onHoverImmediate: () => redrawTrackOverlaysOnly(state),
  onUpdateReadouts: () => scheduler.invalidate("readouts"),
  onUpdateMeasure: () => scheduler.invalidate("inspector"),
  onApplyTrackY: (track: (typeof state.tracks)[number]) => applyTrackYScale(state, track),
  onZoomToRange: (t0: number, t1: number) => zoomToRange(t0, t1),
  onPlaceSampleCursor: (time: number) => {
    state.sampleCursor = time;
    state.sampleCursorPinned = true;
    scheduler.invalidate("readouts", "inspector", "overlays");
  },
  onContextMenu: (req: Parameters<typeof showContextMenu>[1]) => {
    showContextMenu(state, req, contextMenuActions);
  },
};

function rebuild(): void {
  lastStreamRenderEnd = -Infinity;
  state.lastFollowQueryKey = "";
  state.canvasTheme = themeManager.getCanvasTheme();
  rebuildTracks(state, legendCallbacks, interactionCallbacks);
  updateSuggestions(state);
  presetController?.syncDirtyIndicator();
  presetController?.renderEmptyQuickLaunch();
  scheduler.wake();
}

function applyEvent(event: TimelineEvent): void {
  const seconds = event.t / NS;
  if (seconds > 0 && (state.historyStart === 0 || seconds < state.historyStart)) {
    state.historyStart = Math.max(0, seconds);
  }
  state.allEvents.push({ ...event, t: seconds });
  if (state.allEvents.length > 200) {
    state.allEvents.shift();
  }
  if (event.kind === EVENT_HALTED) {
    state.halts.push({ from: seconds, to: null });
  } else if (event.kind === EVENT_RESUMED) {
    const open = state.halts[state.halts.length - 1];
    if (open && open.to === null) open.to = seconds;
  } else if (event.kind === EVENT_RESET || event.kind === EVENT_RELOAD || event.kind === EVENT_FAULT) {
    state.markers.push({ t: seconds, kind: event.kind, detail: event.detail });
  }
  updateEventsUi(state, (ev) => {
    stopFollowing(state);
    state.viewEnd = ev.t + spanSeconds(state.division) * 0.5;
    requestRefresh();
  });
}

interface StatsReply {
  hss: { records: number; lost: number; bytes?: number };
  poll?: { cycles: number; errors: number };
}

interface RecordingInfoReply {
  active: boolean;
  path: string;
  startedNs: number;
  samples: number;
  dropped: number;
}

async function refreshStatus(): Promise<void> {
  if (document.hidden) return;
  try {
    const status = await client.call<{
      nowNs: number;
      connected: boolean;
      device?: string;
      speedKhz?: number;
      recording?: string | null;
      recordingInfo?: RecordingInfoReply | null;
      lastRecordingPath?: string | null;
    }>("session.status");
    const estimate = nowSeconds(state);
    const reported = status.nowNs / NS;
    const drift = Math.abs(estimate - reported);
    if (state.serverNow === 0 || drift > 0.25) {
      state.serverNow = reported;
      state.serverNowAt = performance.now();
    } else if (drift > 0.002) {
      state.serverNow = estimate + (reported - estimate) * 0.2;
      state.serverNowAt = performance.now();
    }
    if (status.recordingInfo?.active) {
      state.recordingStartedAt = status.recordingInfo.startedNs / NS;
      state.recordingSamples = status.recordingInfo.samples;
      state.recordingDropped = status.recordingInfo.dropped;
      syncRecordButton(state, true);
    } else if (typeof status.recording === "string") {
      syncRecordButton(state, true);
    } else if (state.recording) {
      // Stopped externally (e.g. VS Code command palette).
      const endedAt = nowSeconds(state);
      const path = status.lastRecordingPath ?? "";
      if (path) {
        state.lastRecording = {
          path,
          startedAt: state.recordingStartedAt,
          endedAt,
          samples: state.recordingSamples,
          dropped: state.recordingDropped,
        };
        exportUi?.showRecordingBanner();
      }
      state.recordingStartedAt = null;
      syncRecordButton(state, false);
    } else {
      syncRecordButton(state, false);
      if (status.lastRecordingPath && !state.lastRecording) {
        state.lastRecording = {
          path: status.lastRecordingPath,
          startedAt: null,
          endedAt: null,
          samples: 0,
          dropped: 0,
        };
      }
    }
    if (status.connected) {
      setConnectionStatus(`${status.device} @ ${(status.speedKhz ?? 0) / 1000} MHz`, "connected");
    } else {
      setConnectionStatus("No target connected", "disconnected");
    }
    syncFollowButton(state);

    let lostCount = 0;
    try {
      const stats = await client.call<StatsReply>("session.stats");
      lostCount = stats.hss.lost;
    } catch {
      // Non-fatal: keep rate summary visible even if stats are unavailable
    }

    const hssCount = state.series.filter((item) => item.actualMode === "hss").length;
    const pollCount = state.series.filter((item) => item.actualMode === "poll").length;
    const rttCount = state.series.filter((item) => item.actualMode === "rtt").length;
    const parts: string[] = [];
    if (hssCount > 0) {
      const rate = state.series.find((item) => item.actualMode === "hss")?.rateHz ?? 0;
      parts.push(`HSS ${hssCount} @ ${rate} Hz (drop ${lostCount})`);
    }
    if (pollCount > 0) {
      const pRate = state.series.find((item) => item.actualMode === "poll")?.rateHz ?? 0;
      parts.push(`Poll ${pollCount} @ ${pRate} Hz`);
    }
    if (rttCount > 0) {
      parts.push(`RTT ${rttCount}`);
    }
    const ratesEl = document.getElementById("rates");
    if (ratesEl) ratesEl.textContent = parts.join(" │ ");
  } catch {
    setConnectionStatus("Reconnecting…", "reconnecting");
  }
}

async function startSession(): Promise<void> {
  lastStreamRenderEnd = -Infinity;
  state.streamBuffer.clear();
  state.clockPll.reset();
  const reply = await client.call<{ events: TimelineEvent[] }>("events.query", {});
  state.halts = [];
  state.markers = [];
  state.allEvents = [];
  reply.events.forEach(applyEvent);
  await refreshStatus();
  showRate(await client.call<RateReply>("timeline.rate"));
  await presetController?.refreshFromServer();
  await loadSeries();
}

// Empty/blank drop zone creates a new track when dragging signals or RTT signals
const newTrackDrop = document.getElementById("new-track-drop");
const tracksScroll = document.getElementById("tracks-scroll");
for (const zone of [newTrackDrop, tracksScroll]) {
  if (!zone) continue;
  zone.addEventListener("dragover", (event) => {
    if (!state.dragging) return;
    if (zone === tracksScroll && (event.target as HTMLElement).closest(".track-row")) return;
    event.preventDefault();
    newTrackDrop?.classList.add("is-hovered");
  });
  zone.addEventListener("dragleave", () => {
    newTrackDrop?.classList.remove("is-hovered");
  });
  zone.addEventListener("drop", (event) => {
    if (zone === tracksScroll && (event.target as HTMLElement).closest(".track-row")) return;
    event.preventDefault();
    newTrackDrop?.classList.remove("is-hovered");
    if (!state.dragging) return;
    if (state.dragging.kind === "series") {
      void moveSeriesToTrack(state.dragging.seriesId, null);
    } else if (state.dragging.kind === "rtt") {
      void plotRttSignalToTrack(state.dragging.signalName, null);
    }
    state.dragging = null;
    newTrackDrop?.classList.remove("is-active");
  });
}

// Shared bottom axis & Overview scrubber
const sharedAxisCanvas = document.getElementById("shared-axis-canvas") as HTMLCanvasElement | null;
if (sharedAxisCanvas) {
  attachSharedAxisInteractions(sharedAxisCanvas, state, () => {
    scheduler.invalidate("readouts", "inspector", "overlays");
  });
}

attachOverviewScrubber(state, (center) => {
  stopFollowing(state);
  state.viewEnd = center + spanSeconds(state.division) * 0.5;
  syncFollowButton(state);
  requestRefresh();
});

// Wire client, theme, toolbar, context menu, keyboard
client.onSamples = (batches) => {
  state.streamBuffer.appendBatches(batches);
  if (state.followLive) {
    scheduler.wake();
  }
};
client.onEvent = (event) => {
  applyEvent(event);
  if (event.kind !== EVENT_HALTED && event.kind !== EVENT_RESUMED) {
    void loadSeries();
  }
};
client.onConnection = (connected) => {
  if (connected) {
    void startSession();
  } else {
    setConnectionStatus("Reconnecting…", "reconnecting");
  }
};

themeManager.onChange((newCanvasTheme) => {
  state.canvasTheme = newCanvasTheme;
  const select = document.getElementById("theme-select") as HTMLSelectElement | null;
  if (select) select.value = themeManager.getPreference();
  rebuild();
});

exportUi = new ExportUiController(state, {
  zoomToRange,
  setMeasureText,
});

presetController = new PresetController(client, state, {
  loadSeries,
  refresh,
  rebuild,
  showRateReply: showRate,
  setMeasureText,
});

wireToolbar(client, state, themeManager, {
  loadSeries,
  refresh,
  rebuild,
  fitAllHistory,
  resizeAll: () => scheduler.invalidate("layout"),
  clearCursors,
  zoomToRange,
  onFilterChanged: () => applySeriesFilter(state),
  onAppearanceChanged: () => {
    syncTracksAppearance(state);
    presetController?.syncDirtyIndicator();
  },
  onDrawerOpened: (tab) => {
    if (tab === "cursors") {
      scheduler.invalidate("inspector");
    } else {
      renderRttSignalsDrawer(state, legendCallbacks);
    }
  },
  onOpenExport: () => exportUi?.openPopover(),
  onRateChanged: () => presetController?.syncDirtyIndicator(),
  onRecordingStopped: () => exportUi?.showRecordingBanner(),
});

initContextMenuDismiss();

window.addEventListener("keydown", (event) => {
  const action = resolveShortcut({
    key: event.key,
    ctrlKey: event.ctrlKey,
    metaKey: event.metaKey,
    shiftKey: event.shiftKey,
    altKey: event.altKey,
    isInputFocused: isInputElement(event.target),
  });
  if (!action) return;
  if (action === "blurInput") {
    (event.target as HTMLElement | null)?.blur();
    return;
  }

  if (action !== "toggleHelp" && action !== "clearCursors") {
    event.preventDefault();
  }

  const selectedSignal =
    state.series.find((s) => s.seriesId === state.selectedSeriesId) ??
    state.tracks.find((t) => t.name === state.selectedTrack)?.members[0];

  switch (action) {
    case "toggleFollow":
      document.getElementById("follow")?.click();
      break;
    case "panLeftDiv":
      stopFollowing(state);
      state.viewEnd -= state.division;
      syncFollowButton(state);
      requestRefresh();
      break;
    case "panRightDiv":
      stopFollowing(state);
      state.viewEnd = Math.min(nowSeconds(state), state.viewEnd + state.division);
      syncFollowButton(state);
      requestRefresh();
      break;
    case "panLeftScreen":
      stopFollowing(state);
      state.viewEnd -= spanSeconds(state.division);
      syncFollowButton(state);
      requestRefresh();
      break;
    case "panRightScreen":
      stopFollowing(state);
      state.viewEnd = Math.min(nowSeconds(state), state.viewEnd + spanSeconds(state.division));
      syncFollowButton(state);
      requestRefresh();
      break;
    case "jumpStart":
      stopFollowing(state);
      state.viewEnd = Math.max(0, state.historyStart) + spanSeconds(state.division);
      syncFollowButton(state);
      requestRefresh();
      break;
    case "jumpEnd":
      state.followLive = true;
      state.clockPll.reset();
      state.viewEnd = nowSeconds(state) - FOLLOW_LAG_S;
      syncFollowButton(state);
      requestRefresh();
      scheduler.wake();
      break;
    case "zoomIn":
      state.division = stepDivision(state.division, -1);
      syncDivisionControls(state);
      requestRefresh();
      break;
    case "zoomOut":
      state.division = stepDivision(state.division, 1);
      syncDivisionControls(state);
      requestRefresh();
      break;
    case "fitAll":
      fitAllHistory();
      break;
    case "selectPrevTrack":
    case "selectNextTrack": {
      if (state.tracks.length === 0) break;
      const curIdx = state.tracks.findIndex((t) => t.name === state.selectedTrack);
      const delta = action === "selectPrevTrack" ? -1 : 1;
      const nextIdx =
        curIdx < 0
          ? 0
          : Math.max(0, Math.min(state.tracks.length - 1, curIdx + delta));
      const nextTrack = state.tracks[nextIdx];
      state.selectedTrack = nextTrack.name;
      state.selectedSeriesId = nextTrack.members[0]?.seriesId ?? null;
      updateTrackSelectionHighlight(state);
      nextTrack.element.scrollIntoView({ block: "nearest" });
      break;
    }
    case "deleteSelectedSignal":
      if (selectedSignal) void removeSignal(selectedSignal);
      break;
    case "placeCursorA":
      if (state.hoverTime !== null) {
        state.cursorA = state.hoverTime;
        scheduler.invalidate("readouts", "inspector", "overlays");
      }
      break;
    case "placeCursorB":
      if (state.hoverTime !== null) {
        state.cursorB = state.hoverTime;
        scheduler.invalidate("readouts", "inspector", "overlays");
      }
      break;
    case "clearCursors": {
      const help = document.getElementById("help-modal");
      const events = document.getElementById("events-popover");
      const presetPop = document.getElementById("preset-popover");
      const exportPop = document.getElementById("export-popover");
      if (help && !help.hidden) {
        help.hidden = true;
      } else if (presetPop && !presetPop.hidden) {
        presetController?.closePopover();
      } else if (exportPop && !exportPop.hidden) {
        exportUi?.closePopover();
      } else if (events && !events.hidden) {
        events.hidden = true;
      } else {
        clearCursors();
      }
      break;
    }
    case "toggleHideSignal":
      if (selectedSignal) toggleSignalHide(selectedSignal);
      break;
    case "toggleSoloSignal":
      if (selectedSignal) toggleSignalSolo(selectedSignal);
      break;
    case "autoScaleTrackY": {
      const trackName =
        state.selectedTrack ??
        (selectedSignal ? effectiveGroupOf(state, selectedSignal) : null);
      if (trackName) autoScaleTrackY(trackName);
      break;
    }
    case "focusExpr":
      (document.getElementById("expr") as HTMLInputElement | null)?.focus();
      break;
    case "toggleHelp": {
      const help = document.getElementById("help-modal");
      if (help) help.hidden = !help.hidden;
      break;
    }
  }
});

const workspaceEl = document.getElementById("tracks-workspace");
if (workspaceEl) {
  new ResizeObserver(() => scheduler.invalidate("layout")).observe(workspaceEl);
}

document.addEventListener("visibilitychange", () => {
  if (!document.hidden) {
    scheduler.wake();
  }
});

syncDivisionControls(state);
updateSuggestions(state);
client.connect();
setInterval(() => void refreshStatus(), STATUS_MS);
setInterval(() => {
  if (state.recording) syncRecordButton(state, true);
}, 500);
setInterval(() => {
  if (client && !document.hidden) {
    void client
      .call<{ signals: RttSignalInfo[] }>("rtt.signals")
      .then((res) => {
        state.rttSignals = res.signals ?? [];
        renderRttSignalsDrawer(state, legendCallbacks);
      })
      .catch(() => undefined);
  }
}, STATUS_MS);
