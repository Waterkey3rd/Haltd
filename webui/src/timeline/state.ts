import type uPlot from "uplot";
import type { QueryResult, SeriesInfo, TimelineEvent } from "../client";
import type { CanvasTheme } from "../theme";
import { valueAtColumns } from "./math";

export const NS = 1e9;
export const DEFAULT_GROUP = "default";
export const Y_AXIS_SIZE = 64;
export const DEFAULT_TRACK_HEIGHT = 160;
export const MIN_TRACK_HEIGHT = 80;
export const MAX_TRACK_HEIGHT = 600;
export const EXPR_HISTORY_KEY = "haltd.timeline.exprHistory";
export const LEGEND_WIDTH_KEY = "haltd.timeline.legendWidth";
export const TRACK_HEIGHTS_KEY = "haltd.timeline.trackHeights";

export interface Halted {
  from: number;
  to: number | null;
}

export interface YScaleConfig {
  auto: boolean;
  min: number;
  max: number;
  collapsed: boolean;
}

export interface RttSignalInfo {
  name: string;
  value: number | null;
  seriesId: number | null;
}

export interface TrackRow {
  name: string;
  members: SeriesInfo[];
  plot: uPlot;
  element: HTMLElement;
  legendEl: HTMLElement;
  plotEl: HTMLElement;
  canvasWrap: HTMLElement;
  legendValues: Map<number, HTMLElement>;
  legendCards: Map<number, HTMLElement>;
  yAutoBtn: HTMLButtonElement;
  yRangeWrap: HTMLElement;
  yMinInput: HTMLInputElement;
  yMaxInput: HTMLInputElement;
  heightPx: number;
}

export interface TimelineState {
  canvasTheme: CanvasTheme;
  series: SeriesInfo[];
  rttSignals: RttSignalInfo[];
  hidden: Set<number>;
  soloSeriesId: number | null;
  selectedSeriesId: number | null;
  selectedTrack: string | null;
  tracks: TrackRow[];
  groupYScales: Map<string, YScaleConfig>;
  trackHeights: Map<string, number>;
  followLive: boolean;
  division: number;
  splitLanes: boolean;
  showBands: boolean;
  seriesFilter: string;
  viewEnd: number;
  historyStart: number;
  serverNow: number;
  serverNowAt: number;
  halts: Halted[];
  markers: TimelineEvent[];
  allEvents: TimelineEvent[];
  cursorA: number | null;
  cursorB: number | null;
  sampleCursor: number | null;
  sampleCursorPinned: boolean;
  hoverTime: number | null;
  boxSelect: { groupName: string; x0: number; x1: number } | null;
  querying: boolean;
  lastQuery: number;
  lastColumns: { result: QueryResult; xs: number[] } | null;
  recording: boolean;
  exprHistory: string[];
  /** Currently dragged signal or RTT signal name. */
  dragging:
    | { kind: "series"; seriesId: number }
    | { kind: "rtt"; signalName: string }
    | null;
}

export function loadExprHistory(): string[] {
  try {
    const raw = localStorage.getItem(EXPR_HISTORY_KEY);
    if (raw) {
      const parsed = JSON.parse(raw);
      if (Array.isArray(parsed)) {
        return parsed.filter((x): x is string => typeof x === "string").slice(0, 24);
      }
    }
  } catch {
    // Ignore storage errors
  }
  return [];
}

export function saveExprHistory(history: string[]): void {
  try {
    localStorage.setItem(EXPR_HISTORY_KEY, JSON.stringify(history));
  } catch {
    // Ignore storage errors
  }
}

export function loadTrackHeights(): Map<string, number> {
  const map = new Map<string, number>();
  try {
    const raw = localStorage.getItem(TRACK_HEIGHTS_KEY);
    if (raw) {
      const parsed = JSON.parse(raw) as Record<string, unknown>;
      for (const [k, v] of Object.entries(parsed)) {
        if (typeof v === "number" && Number.isFinite(v)) {
          map.set(k, Math.max(MIN_TRACK_HEIGHT, Math.min(MAX_TRACK_HEIGHT, Math.round(v))));
        }
      }
    }
  } catch {
    // Ignore storage errors
  }
  return map;
}

export function saveTrackHeights(heights: Map<string, number>): void {
  try {
    const obj: Record<string, number> = {};
    for (const [k, v] of heights.entries()) {
      obj[k] = v;
    }
    localStorage.setItem(TRACK_HEIGHTS_KEY, JSON.stringify(obj));
  } catch {
    // Ignore storage errors
  }
}

export function createInitialState(canvasTheme: CanvasTheme): TimelineState {
  return {
    canvasTheme,
    series: [],
    rttSignals: [],
    hidden: new Set(),
    soloSeriesId: null,
    selectedSeriesId: null,
    selectedTrack: null,
    tracks: [],
    groupYScales: new Map(),
    trackHeights: loadTrackHeights(),
    followLive: true,
    division: 0.5,
    splitLanes: false,
    showBands: true,
    seriesFilter: "",
    viewEnd: 0,
    historyStart: 0,
    serverNow: 0,
    serverNowAt: performance.now(),
    halts: [],
    markers: [],
    allEvents: [],
    cursorA: null,
    cursorB: null,
    sampleCursor: null,
    sampleCursorPinned: false,
    hoverTime: null,
    boxSelect: null,
    querying: false,
    lastQuery: 0,
    lastColumns: null,
    recording: false,
    exprHistory: loadExprHistory(),
    dragging: null,
  };
}

export function nowSeconds(state: TimelineState): number {
  return state.serverNow + (performance.now() - state.serverNowAt) / 1000;
}

export function isSeriesVisible(state: TimelineState, item: SeriesInfo): boolean {
  if (state.soloSeriesId !== null) {
    return item.seriesId === state.soloSeriesId;
  }
  return !state.hidden.has(item.seriesId);
}

export function effectiveGroupOf(state: TimelineState, item: SeriesInfo): string {
  if (state.splitLanes) {
    return item.name;
  }
  return item.group && item.group.trim() ? item.group.trim() : DEFAULT_GROUP;
}

export function getGroupYScale(state: TimelineState, name: string): YScaleConfig {
  let cfg = state.groupYScales.get(name);
  if (!cfg) {
    cfg = { auto: true, min: -1, max: 1, collapsed: false };
    state.groupYScales.set(name, cfg);
  }
  return cfg;
}

export function colorOf(state: TimelineState, item: SeriesInfo, index: number): string {
  const palette = state.canvasTheme.palette;
  return item.color ?? palette[Math.max(0, index) % palette.length];
}

export function toHexColor(color: string): string {
  if (/^#[0-9a-fA-F]{6}$/.test(color)) return color;
  if (/^#[0-9a-fA-F]{8}$/.test(color)) return color.slice(0, 7);
  return "#4dabf7";
}

export function withAlpha(color: string, alphaHex: string): string {
  return toHexColor(color) + alphaHex;
}

export function valueAt(state: TimelineState, item: SeriesInfo, seconds: number): number | null {
  if (!state.lastColumns) {
    return null;
  }
  return valueAtColumns(
    state.lastColumns.xs,
    state.lastColumns.result.series.get(item.seriesId),
    seconds,
  );
}

export function legendReadoutTime(state: TimelineState): number | null {
  if (state.hoverTime !== null) {
    return state.hoverTime;
  }
  if (state.sampleCursor !== null) {
    return state.sampleCursor;
  }
  return null;
}

export function nextCustomTrackName(state: TimelineState, baseHint?: string): string {
  const existing = new Set(state.series.map((s) => effectiveGroupOf(state, s)));
  const cleanHint = baseHint?.replace(/[^A-Za-z0-9_:.]/g, "_") ?? "";
  if (cleanHint && !existing.has(cleanHint)) {
    return cleanHint;
  }
  let idx = 1;
  while (existing.has(`track-${idx}`)) {
    idx++;
  }
  return `track-${idx}`;
}
