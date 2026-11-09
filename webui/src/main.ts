import uPlot from "uplot";
import "uplot/dist/uPlot.min.css";
import "./style.css";
import {
  Client,
  EVENT_FAULT,
  EVENT_HALTED,
  EVENT_RELOAD,
  EVENT_RESET,
  EVENT_RESUMED,
  EVENT_SAMPLING,
  type QueryResult,
  type SeriesInfo,
  type TimelineEvent,
} from "./client";
import { ThemeManager, type CanvasTheme, type ThemePreference } from "./theme";

const NS = 1e9;
const REFRESH_MS = 33;
const STATUS_MS = 1000;
/** Seconds per division, in the order the wheel steps through them. */
const DIVISIONS = [0.001, 0.002, 0.005, 0.01, 0.02, 0.05, 0.1, 0.2, 0.5, 1, 2, 5, 10, 30];
const DIVISIONS_ON_SCREEN = 10;
/** Wheel travel for one step, so that a trackpad does not race through them. */
const WHEEL_STEP = 60;
/** Individual samples get a marker at this zoom and below. */
const MARKERS_AT_DIVISION = 0.02;
/** Newest data is held back slightly so the right edge is not ragged. */
const FOLLOW_LAG_S = 0.03;
const DEFAULT_GROUP = "default";
const EXPR_HISTORY_KEY = "jlinkd.timeline.exprHistory";
const SIDEBAR_WIDTH_KEY = "jlinkd.timeline.sidebarWidth";

const byId = <T extends HTMLElement>(id: string) => document.getElementById(id) as T;

interface Halted {
  from: number;
  to: number | null;
}

interface YScaleConfig {
  auto: boolean;
  min: number;
  max: number;
  collapsed: boolean;
}

interface GroupPlot {
  name: string;
  members: SeriesInfo[];
  plot: uPlot;
  element: HTMLElement;
  canvasWrap: HTMLElement;
  legendValues: Map<number, HTMLElement>;
  yAutoBtn: HTMLButtonElement;
  yRangeWrap: HTMLElement;
  yMinInput: HTMLInputElement;
  yMaxInput: HTMLInputElement;
}

interface SignalDomRefs {
  liveVal: HTMLElement;
  minVal: HTMLElement;
  maxVal: HTMLElement;
  avgVal: HTMLElement;
  ppVal: HTMLElement;
}

interface RttSignalInfo {
  name: string;
  value: number | null;
  seriesId: number | null;
}

const themeManager = new ThemeManager();
let canvasTheme: CanvasTheme = themeManager.getCanvasTheme();

const client = new Client();
let series: SeriesInfo[] = [];
const hidden = new Set<number>();
let soloSeriesId: number | null = null;
let groups: GroupPlot[] = [];
const groupYScales = new Map<string, YScaleConfig>();
const signalDomRefs = new Map<number, SignalDomRefs>();

let followLive = true;
let division = 0.5;
let splitLanes = false;
let showBands = true;
let seriesFilter = "";

/** Right edge of the view, in seconds since the backend started. */
let viewEnd = 0;
let historyStart = 0;
/** Server clock: seconds = serverNow + (performance.now() - serverNowAt) / 1000. */
let serverNow = 0;
let serverNowAt = performance.now();
let halts: Halted[] = [];
let markers: TimelineEvent[] = [];
let allEvents: TimelineEvent[] = [];
let cursorA: number | null = null;
let cursorB: number | null = null;
let hoverTime: number | null = null;
let boxSelect: { groupName: string; x0: number; x1: number } | null = null;
let querying = false;
let lastQuery = 0;
let lastColumns: { result: QueryResult; xs: number[] } | null = null;
let recording = false;
let rttSignals: RttSignalInfo[] = [];
let exprHistory: string[] = loadExprHistory();

const nowSeconds = () => serverNow + (performance.now() - serverNowAt) / 1000;
const spanSeconds = () => division * DIVISIONS_ON_SCREEN;

function loadExprHistory(): string[] {
  try {
    const raw = localStorage.getItem(EXPR_HISTORY_KEY);
    if (raw) {
      const parsed = JSON.parse(raw);
      if (Array.isArray(parsed)) return parsed.filter((x): x is string => typeof x === "string").slice(0, 24);
    }
  } catch {
    // Ignore storage errors
  }
  return [];
}

function rememberExpr(expr: string): void {
  const clean = expr.trim();
  if (!clean) return;
  exprHistory = [clean, ...exprHistory.filter((item) => item !== clean)].slice(0, 24);
  try {
    localStorage.setItem(EXPR_HISTORY_KEY, JSON.stringify(exprHistory));
  } catch {
    // Ignore storage errors
  }
  updateSuggestions();
}

function updateSuggestions(): void {
  const exprList = byId<HTMLDataListElement>("expr-suggestions");
  const mode = byId<HTMLSelectElement>("mode").value;
  const items = mode === "rtt" ? rttSignals.map((s) => s.name) : exprHistory;
  exprList.replaceChildren(...items.map((val) => new Option(val, val)));

  const groupList = byId<HTMLDataListElement>("group-suggestions");
  const groupNames = [...new Set(series.map((s) => s.group).filter((g): g is string => Boolean(g)))];
  groupList.replaceChildren(...groupNames.map((g) => new Option(g, g)));
}

function colorOf(item: SeriesInfo, index: number): string {
  const palette = canvasTheme.palette;
  return item.color ?? palette[index % palette.length];
}

function toHexColor(color: string): string {
  if (/^#[0-9a-fA-F]{6}$/.test(color)) return color;
  if (/^#[0-9a-fA-F]{8}$/.test(color)) return color.slice(0, 7);
  return "#4dabf7";
}

function withAlpha(color: string, alphaHex: string): string {
  const hex = toHexColor(color);
  return hex + alphaHex;
}

function formatNumber(val: number | null | undefined, sigDigits = 5): string {
  if (val === null || val === undefined || Number.isNaN(val)) return "—";
  const abs = Math.abs(val);
  if (abs === 0) return "0";
  if (abs >= 1e6 || (abs > 0 && abs < 1e-4)) return val.toExponential(3);
  if (Number.isInteger(val) && abs < 1e6) return String(val);
  return Number(val.toPrecision(sigDigits)).toString();
}

function formatSeconds(seconds: number): string {
  const size = Math.abs(seconds);
  if (size >= 1) return `${seconds.toFixed(3)} s`;
  if (size >= 1e-3) return `${(seconds * 1e3).toFixed(3)} ms`;
  return `${(seconds * 1e6).toFixed(1)} µs`;
}

function formatFrequency(deltaSeconds: number): string {
  const abs = Math.abs(deltaSeconds);
  if (abs <= 1e-9) return "—";
  const hz = 1 / abs;
  if (hz >= 1e6) return `${(hz / 1e6).toFixed(2)} MHz`;
  if (hz >= 1e3) return `${(hz / 1e3).toFixed(2)} kHz`;
  return `${hz.toFixed(2)} Hz`;
}

function eventKindName(kind: number): { label: string; cls: string; color: () => string } {
  switch (kind) {
    case EVENT_HALTED:
      return { label: "HALT", cls: "halt", color: () => canvasTheme.haltStroke };
    case EVENT_RESUMED:
      return { label: "RESUME", cls: "reload", color: () => canvasTheme.samplingColor };
    case EVENT_RESET:
      return { label: "RESET", cls: "reset", color: () => canvasTheme.resetColor };
    case EVENT_RELOAD:
      return { label: "RELOAD", cls: "reload", color: () => canvasTheme.reloadColor };
    case EVENT_FAULT:
      return { label: "FAULT", cls: "fault", color: () => canvasTheme.faultColor };
    case EVENT_SAMPLING:
    default:
      return { label: "CFG", cls: "reload", color: () => canvasTheme.samplingColor };
  }
}

function applyEvent(event: TimelineEvent): void {
  const seconds = event.t / NS;
  if (seconds > 0 && (historyStart === 0 || seconds < historyStart)) {
    historyStart = Math.max(0, seconds);
  }
  allEvents.push({ ...event, t: seconds });
  if (allEvents.length > 200) {
    allEvents.shift();
  }
  if (event.kind === EVENT_HALTED) {
    halts.push({ from: seconds, to: null });
  } else if (event.kind === EVENT_RESUMED) {
    const open = halts[halts.length - 1];
    if (open && open.to === null) {
      open.to = seconds;
    }
  } else if (event.kind === EVENT_RESET || event.kind === EVENT_RELOAD || event.kind === EVENT_FAULT) {
    markers.push({ t: seconds, kind: event.kind, detail: event.detail });
  }
  updateEventsUi();
}

function updateEventsUi(): void {
  const notable = allEvents.filter(
    (e) =>
      e.kind === EVENT_HALTED ||
      e.kind === EVENT_RESET ||
      e.kind === EVENT_RELOAD ||
      e.kind === EVENT_FAULT,
  );
  const btn = byId<HTMLButtonElement>("events-btn");
  btn.textContent = `Events: ${notable.length}`;

  const list = byId("events-list");
  if (notable.length === 0) {
    list.innerHTML = `<div style="padding:8px;color:var(--muted);text-align:center;">No target events recorded yet.</div>`;
    return;
  }
  list.replaceChildren(
    ...notable
      .slice()
      .reverse()
      .slice(0, 60)
      .map((ev) => {
        const info = eventKindName(ev.kind);
        const row = document.createElement("div");
        row.className = "event-row";
        row.title = "Click to jump Timeline to this event";
        const tag = document.createElement("span");
        tag.className = `event-tag ${info.cls}`;
        tag.textContent = info.label;
        const time = document.createElement("span");
        time.textContent = `t = ${ev.t.toFixed(4)} s`;
        row.append(tag, time);
        row.onclick = () => {
          stopFollowing();
          viewEnd = ev.t + spanSeconds() * 0.5;
          void refresh();
        };
        return row;
      }),
  );
}

function drawBadge(
  ctx: CanvasRenderingContext2D,
  x: number,
  y: number,
  text: string,
  bgColor: string,
  textColor: string,
  align: "center" | "left" | "right" = "center",
): void {
  const dpr = devicePixelRatio || 1;
  ctx.save();
  ctx.font = `600 ${Math.round(10 * dpr)}px ui-monospace, monospace`;
  const padX = 4 * dpr;
  const padY = 2 * dpr;
  const metrics = ctx.measureText(text);
  const w = metrics.width + padX * 2;
  const h = 13 * dpr + padY;
  let bx = x - w / 2;
  if (align === "left") bx = x + 2 * dpr;
  if (align === "right") bx = x - w - 2 * dpr;
  ctx.fillStyle = bgColor;
  ctx.beginPath();
  ctx.roundRect(bx, y, w, h, 3 * dpr);
  ctx.fill();
  ctx.fillStyle = textColor;
  ctx.textBaseline = "middle";
  ctx.textAlign = "left";
  ctx.fillText(text, bx + padX, y + h / 2);
  ctx.restore();
}

function drawOverlays(groupName: string, members: SeriesInfo[]) {
  return (plot: uPlot): void => {
    const context = plot.ctx;
    const { left, top, width, height } = plot.bbox;
    const min = plot.scales.x.min ?? 0;
    const max = plot.scales.x.max ?? 0;
    const dpr = devicePixelRatio || 1;
    const toX = (value: number) => plot.valToPos(value, "x", true);
    const toY = (value: number) => plot.valToPos(value, "y", true);

    context.save();
    context.beginPath();
    context.rect(left, top, width, height);
    context.clip();

    // 1. Halted regions
    for (const halt of halts) {
      const end = halt.to ?? max;
      if (end >= min && halt.from <= max) {
        const x0 = toX(Math.max(halt.from, min));
        const x1 = toX(Math.min(end, max));
        const w = Math.max(1, x1 - x0);
        context.fillStyle = canvasTheme.haltFill;
        context.fillRect(x0, top, w, height);
        context.fillStyle = canvasTheme.haltStroke;
        context.fillRect(x0, top, w, Math.max(2, 2 * dpr));
        if (w > 54 * dpr) {
          const dur = (halt.to ?? nowSeconds()) - halt.from;
          drawBadge(
            context,
            x0 + w / 2,
            top + 4 * dpr,
            `HALT ${formatSeconds(dur)}`,
            canvasTheme.haltStroke,
            "#ffffff",
          );
        }
      }
    }

    // 2. Box-Zoom selection preview
    if (boxSelect && boxSelect.groupName === groupName) {
      const bx0 = Math.min(boxSelect.x0, boxSelect.x1);
      const bw = Math.abs(boxSelect.x1 - boxSelect.x0);
      context.fillStyle = canvasTheme.selectionFill;
      context.fillRect(bx0, top, bw, height);
      context.strokeStyle = canvasTheme.selectionStroke;
      context.lineWidth = dpr;
      context.strokeRect(bx0 + 0.5, top + 0.5, bw, height - 1);
    }

    // 3. Cursor A..B shaded span
    if (cursorA !== null && cursorB !== null) {
      const cMin = Math.min(cursorA, cursorB);
      const cMax = Math.max(cursorA, cursorB);
      if (cMax >= min && cMin <= max) {
        const x0 = toX(Math.max(cMin, min));
        const x1 = toX(Math.min(cMax, max));
        context.fillStyle = canvasTheme.cursorSpanFill;
        context.fillRect(x0, top, x1 - x0, height);
      }
    }

    context.lineWidth = dpr;
    const vline = (value: number, style: string, dashed = false) => {
      if (value >= min && value <= max) {
        const x = Math.round(toX(value)) + 0.5;
        context.save();
        context.strokeStyle = style;
        if (dashed) context.setLineDash([4 * dpr, 3 * dpr]);
        context.beginPath();
        context.moveTo(x, top);
        context.lineTo(x, top + height);
        context.stroke();
        context.restore();
        return x;
      }
      return null;
    };

    // 4. Event Markers (Fault / Reset / Reload)
    for (const marker of markers) {
      const info = eventKindName(marker.kind);
      const x = vline(marker.t, info.color(), marker.kind !== EVENT_FAULT);
      if (x !== null) {
        drawBadge(context, x, top + 3 * dpr, info.label, info.color(), canvasTheme.badgeText, "left");
      }
    }

    // 5. Hover Crosshair
    if (hoverTime !== null && hoverTime >= min && hoverTime <= max) {
      const hx = vline(hoverTime, canvasTheme.crosshair, true);
      if (hx !== null) {
        for (const member of members) {
          const v = valueAt(member, hoverTime);
          if (v !== null) {
            const py = toY(v);
            if (py >= top && py <= top + height) {
              const color = colorOf(member, series.indexOf(member));
              context.fillStyle = color;
              context.beginPath();
              context.arc(hx, py, 3.5 * dpr, 0, Math.PI * 2);
              context.fill();
            }
          }
        }
      }
    }

    // 6. Cursors A & B
    if (cursorA !== null) {
      const xa = vline(cursorA, canvasTheme.cursorA);
      if (xa !== null) {
        drawBadge(context, xa, top + 2 * dpr, "A", canvasTheme.cursorA, "#ffffff", "right");
        for (const member of members) {
          const v = valueAt(member, cursorA);
          if (v !== null) {
            const py = toY(v);
            if (py >= top && py <= top + height) {
              context.fillStyle = canvasTheme.cursorA;
              context.beginPath();
              context.arc(xa, py, 3.5 * dpr, 0, Math.PI * 2);
              context.fill();
            }
          }
        }
      }
    }
    if (cursorB !== null) {
      const xb = vline(cursorB, canvasTheme.cursorB);
      if (xb !== null) {
        drawBadge(context, xb, top + 2 * dpr, "B", canvasTheme.cursorB, "#ffffff", "left");
        for (const member of members) {
          const v = valueAt(member, cursorB);
          if (v !== null) {
            const py = toY(v);
            if (py >= top && py <= top + height) {
              context.fillStyle = canvasTheme.cursorB;
              context.beginPath();
              context.arc(xb, py, 3.5 * dpr, 0, Math.PI * 2);
              context.fill();
            }
          }
        }
      }
    }

    if (cursorA !== null && cursorB !== null && cursorA >= min && cursorB >= min && cursorA <= max && cursorB <= max) {
      const midX = (toX(cursorA) + toX(cursorB)) / 2;
      const dt = cursorB - cursorA;
      if (Math.abs(toX(cursorB) - toX(cursorA)) > 75 * dpr) {
        drawBadge(
          context,
          midX,
          top + 2 * dpr,
          `Δt ${formatSeconds(dt)} (${formatFrequency(dt)})`,
          canvasTheme.cursorA,
          "#ffffff",
        );
      }
    }

    context.restore();
  };
}

function updateDivisionSelect(): void {
  byId<HTMLSelectElement>("division").value = String(division);
  byId("span-label").textContent = formatSeconds(spanSeconds());
}

function stepDivision(steps: number): void {
  const index = DIVISIONS.indexOf(division);
  const currentIdx = index >= 0 ? index : 8;
  division = DIVISIONS[Math.min(DIVISIONS.length - 1, Math.max(0, currentIdx + steps))];
  updateDivisionSelect();
}

/** Grid lines on exact multiples of the division. */
function divisionSplits(_plot: uPlot, _axis: number, min: number, max: number): number[] {
  const splits: number[] = [];
  const epsilon = division * 1e-6;
  for (let tick = Math.ceil((min - epsilon) / division); tick * division <= max + epsilon; tick++) {
    splits.push(tick * division);
  }
  return splits;
}

function divisionLabels(_plot: uPlot, splits: number[]): string[] {
  const decimals = division >= 1 ? 0 : Math.ceil(-Math.log10(division) - 1e-9);
  return splits.map((value) => value.toFixed(decimals));
}

function stopFollowing(): void {
  if (followLive) {
    viewEnd = nowSeconds() - FOLLOW_LAG_S;
  }
  followLive = false;
  syncFollowButton();
}

function syncFollowButton(): void {
  const button = byId<HTMLButtonElement>("follow");
  if (followLive) {
    button.textContent = "Following";
  } else {
    const lag = Math.max(0, nowSeconds() - viewEnd);
    button.textContent = lag > 0.05 ? `Paused (-${formatSeconds(lag)})` : "Paused";
  }
  button.setAttribute("aria-pressed", String(followLive));
}

function getGroupYScale(name: string): YScaleConfig {
  let cfg = groupYScales.get(name);
  if (!cfg) {
    cfg = { auto: true, min: -1, max: 1, collapsed: false };
    groupYScales.set(name, cfg);
  }
  return cfg;
}

function syncGroupYControls(group: GroupPlot): void {
  const cfg = getGroupYScale(group.name);
  group.yAutoBtn.textContent = cfg.auto ? "Auto Y" : "Locked Y";
  group.yAutoBtn.setAttribute("aria-pressed", String(!cfg.auto));
  group.yRangeWrap.hidden = cfg.auto;
  if (!cfg.auto) {
    if (document.activeElement !== group.yMinInput) {
      group.yMinInput.value = formatNumber(cfg.min, 4);
    }
    if (document.activeElement !== group.yMaxInput) {
      group.yMaxInput.value = formatNumber(cfg.max, 4);
    }
  }
}

function applyGroupYScale(group: GroupPlot): void {
  const cfg = getGroupYScale(group.name);
  syncGroupYControls(group);
  if (!cfg.auto && cfg.max > cfg.min) {
    group.plot.setScale("y", { min: cfg.min, max: cfg.max });
  }
}

function attachInteractions(group: GroupPlot): void {
  const { plot, name: groupName } = group;
  const over = plot.over;
  let drag:
    | { mode: "pan-x"; x: number; end: number }
    | { mode: "pan-y"; y: number; yMin: number; yMax: number }
    | { mode: "box"; x0: number }
    | { mode: "cursor-a" }
    | { mode: "cursor-b" }
    | null = null;
  let moved = false;
  let wheelTravel = 0;

  const isNearCursor = (clientX: number, cursorVal: number | null): boolean => {
    if (cursorVal === null) return false;
    const rect = over.getBoundingClientRect();
    const px = plot.valToPos(cursorVal, "x", false);
    return Math.abs(clientX - rect.left - px) <= 6;
  };

  over.addEventListener(
    "wheel",
    (event) => {
      event.preventDefault();
      const rect = over.getBoundingClientRect();

      // Shift + Wheel: Zoom Y-axis of this subplot
      if (event.shiftKey) {
        const yCfg = getGroupYScale(groupName);
        const curMin = yCfg.auto ? (plot.scales.y.min ?? -1) : yCfg.min;
        const curMax = yCfg.auto ? (plot.scales.y.max ?? 1) : yCfg.max;
        const span = curMax - curMin || 1;
        const fracY = 1 - Math.max(0, Math.min(1, (event.clientY - rect.top) / Math.max(1, rect.height)));
        const pivotY = curMin + span * fracY;
        const factor = event.deltaY > 0 ? 1.2 : 1 / 1.2;
        const newSpan = span * factor;
        yCfg.auto = false;
        yCfg.min = pivotY - newSpan * fracY;
        yCfg.max = pivotY + newSpan * (1 - fracY);
        applyGroupYScale(group);
        return;
      }

      // Standard Wheel: Zoom Time/Division around mouse X
      wheelTravel += event.deltaY;
      if (Math.abs(wheelTravel) < WHEEL_STEP) return;
      const steps = Math.sign(wheelTravel);
      wheelTravel = 0;
      const fraction = Math.max(0, Math.min(1, (event.clientX - rect.left) / Math.max(1, rect.width)));
      const end = followLive ? nowSeconds() - FOLLOW_LAG_S : viewEnd;
      const pivot = end - spanSeconds() * (1 - fraction);
      stopFollowing();
      stepDivision(steps);
      viewEnd = pivot + spanSeconds() * (1 - fraction);
      void refresh();
    },
    { passive: false },
  );

  over.addEventListener("mousedown", (event) => {
    if (event.button !== 0) return;
    const rect = over.getBoundingClientRect();
    moved = false;

    if (event.ctrlKey || event.metaKey) {
      const x0 = (event.clientX - rect.left) * (devicePixelRatio || 1) + plot.bbox.left;
      drag = { mode: "box", x0 };
      boxSelect = { groupName, x0, x1: x0 };
      return;
    }

    if (event.shiftKey) {
      const yCfg = getGroupYScale(groupName);
      const yMin = yCfg.auto ? (plot.scales.y.min ?? -1) : yCfg.min;
      const yMax = yCfg.auto ? (plot.scales.y.max ?? 1) : yCfg.max;
      drag = { mode: "pan-y", y: event.clientY, yMin, yMax };
      return;
    }

    if (isNearCursor(event.clientX, cursorA)) {
      drag = { mode: "cursor-a" };
      moved = true;
      return;
    }
    if (isNearCursor(event.clientX, cursorB)) {
      drag = { mode: "cursor-b" };
      moved = true;
      return;
    }

    drag = { mode: "pan-x", x: event.clientX, end: followLive ? nowSeconds() - FOLLOW_LAG_S : viewEnd };
  });

  over.addEventListener("mousemove", (event) => {
    const rect = over.getBoundingClientRect();
    const relX = Math.max(0, Math.min(rect.width, event.clientX - rect.left));
    hoverTime = plot.posToVal(relX, "x");
    updateHoverReadouts();

    if (!drag) {
      if (isNearCursor(event.clientX, cursorA) || isNearCursor(event.clientX, cursorB)) {
        over.style.cursor = "ew-resize";
      } else if (event.ctrlKey) {
        over.style.cursor = "crosshair";
      } else {
        over.style.cursor = "default";
      }
      redrawAllOverlays();
    }
  });

  over.addEventListener("mouseleave", () => {
    if (!drag) {
      hoverTime = null;
      updateHoverReadouts();
      redrawAllOverlays();
    }
  });

  over.addEventListener("dblclick", () => {
    const yCfg = getGroupYScale(groupName);
    if (!yCfg.auto) {
      yCfg.auto = true;
      syncGroupYControls(group);
      void refresh();
    } else {
      clearCursors();
    }
  });

  window.addEventListener("mousemove", (event) => {
    if (!drag) return;
    const rect = over.getBoundingClientRect();
    const relX = Math.max(0, Math.min(rect.width, event.clientX - rect.left));

    if (drag.mode === "cursor-a") {
      cursorA = plot.posToVal(relX, "x");
      updateMeasure();
      redrawAllOverlays();
      return;
    }
    if (drag.mode === "cursor-b") {
      cursorB = plot.posToVal(relX, "x");
      updateMeasure();
      redrawAllOverlays();
      return;
    }
    if (drag.mode === "box") {
      moved = true;
      const x1 = relX * (devicePixelRatio || 1) + plot.bbox.left;
      boxSelect = { groupName, x0: drag.x0, x1 };
      plot.redraw(false);
      return;
    }
    if (drag.mode === "pan-y") {
      const dy = event.clientY - drag.y;
      if (Math.abs(dy) > 2) moved = true;
      if (moved) {
        const yCfg = getGroupYScale(groupName);
        const span = drag.yMax - drag.yMin || 1;
        const shift = (dy / Math.max(1, rect.height)) * span;
        yCfg.auto = false;
        yCfg.min = drag.yMin + shift;
        yCfg.max = drag.yMax + shift;
        applyGroupYScale(group);
      }
      return;
    }
    if (drag.mode === "pan-x") {
      const delta = event.clientX - drag.x;
      if (Math.abs(delta) > 3) moved = true;
      if (moved) {
        stopFollowing();
        viewEnd = drag.end - (delta / Math.max(1, over.clientWidth)) * spanSeconds();
        syncFollowButton();
      }
    }
  });

  window.addEventListener("mouseup", (event) => {
    if (!drag) return;
    if (drag.mode === "box" && boxSelect) {
      const dpr = devicePixelRatio || 1;
      const p0 = (Math.min(boxSelect.x0, boxSelect.x1) - plot.bbox.left) / dpr;
      const p1 = (Math.max(boxSelect.x0, boxSelect.x1) - plot.bbox.left) / dpr;
      boxSelect = null;
      if (p1 - p0 > 8) {
        const t0 = plot.posToVal(p0, "x");
        const t1 = plot.posToVal(p1, "x");
        zoomToRange(t0, t1);
      } else {
        plot.redraw(false);
      }
      drag = null;
      return;
    }
    if (!moved && event.target === over) {
      placeCursor(plot.posToVal(event.offsetX, "x"));
    }
    drag = null;
  });
}

function redrawAllOverlays(): void {
  for (const g of groups) {
    g.plot.redraw(false);
  }
  drawOverview();
}

function zoomToRange(tA: number, tB: number): void {
  const low = Math.min(tA, tB);
  const high = Math.max(tA, tB);
  const span = Math.max(0.001, high - low);
  const targetDiv = span / DIVISIONS_ON_SCREEN;
  let best = DIVISIONS[0];
  for (const d of DIVISIONS) {
    best = d;
    if (d >= targetDiv) break;
  }
  stopFollowing();
  division = best;
  updateDivisionSelect();
  const center = (low + high) / 2;
  viewEnd = center + spanSeconds() / 2;
  void refresh();
}

function placeCursor(value: number): void {
  if (cursorA === null || cursorB !== null) {
    cursorA = value;
    cursorB = null;
  } else {
    cursorB = value;
  }
  updateMeasure();
  redrawAllOverlays();
}

function clearCursors(): void {
  cursorA = null;
  cursorB = null;
  updateMeasure();
  redrawAllOverlays();
}

function valueAt(item: SeriesInfo, seconds: number): number | null {
  if (!lastColumns) return null;
  const { result, xs } = lastColumns;
  const data = result.series.get(item.seriesId);
  if (!data || xs.length === 0) return null;
  const step = xs.length > 1 ? xs[1] - xs[0] : 1;
  const column = Math.min(xs.length - 1, Math.max(0, Math.round((seconds - xs[0]) / step)));
  for (let distance = 0; distance < xs.length; distance++) {
    for (const index of [column - distance, column + distance]) {
      if (index >= 0 && index < xs.length && !Number.isNaN(data.min[index])) {
        return (data.min[index] + data.max[index]) / 2;
      }
    }
  }
  return null;
}

function computeSeriesStats(item: SeriesInfo): {
  last: number | null;
  min: number | null;
  max: number | null;
  avg: number | null;
  pp: number | null;
} {
  if (!lastColumns) return { last: null, min: null, max: null, avg: null, pp: null };
  const data = lastColumns.result.series.get(item.seriesId);
  if (!data) return { last: null, min: null, max: null, avg: null, pp: null };
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
  if (count === 0) return { last: null, min: null, max: null, avg: null, pp: null };
  return {
    last,
    min,
    max,
    avg: sum / count,
    pp: max - min,
  };
}

function updateHoverReadouts(): void {
  const hoverEl = byId("hover-readout");
  if (hoverTime !== null) {
    hoverEl.textContent = `t = ${hoverTime.toFixed(4)} s`;
  } else {
    hoverEl.textContent = "";
  }

  for (const item of series) {
    const stats = computeSeriesStats(item);
    const displayVal = hoverTime !== null ? valueAt(item, hoverTime) : stats.last;
    const formatted = formatNumber(displayVal, 6);

    const refs = signalDomRefs.get(item.seriesId);
    if (refs) {
      refs.liveVal.textContent = formatted;
      refs.minVal.textContent = formatNumber(stats.min, 4);
      refs.maxVal.textContent = formatNumber(stats.max, 4);
      refs.avgVal.textContent = formatNumber(stats.avg, 4);
      refs.ppVal.textContent = formatNumber(stats.pp, 4);
    }

    for (const group of groups) {
      const legendVal = group.legendValues.get(item.seriesId);
      if (legendVal) {
        legendVal.textContent = formatted;
      }
    }
  }
}

function updateMeasure(): void {
  const output = byId("measure");
  const aEl = byId("cursor-a-val");
  const bEl = byId("cursor-b-val");
  const dtEl = byId("cursor-dt-val");
  const freqEl = byId("cursor-freq-val");
  const zoomBtn = byId<HTMLButtonElement>("cursor-zoom");
  const clearBtn = byId<HTMLButtonElement>("cursor-clear");
  const tbody = byId<HTMLTableSectionElement>("cursor-tbody");

  aEl.textContent = cursorA !== null ? `${cursorA.toFixed(4)} s` : "—";
  bEl.textContent = cursorB !== null ? `${cursorB.toFixed(4)} s` : "—";
  clearBtn.hidden = cursorA === null && cursorB === null;
  zoomBtn.hidden = cursorA === null || cursorB === null;

  const visibleSeries = series.filter((item) => isSeriesVisible(item));

  if (cursorA === null) {
    dtEl.textContent = "—";
    freqEl.textContent = "—";
    output.textContent = "Click a plot to place cursor A, then B.";
    tbody.replaceChildren();
    return;
  }

  if (cursorB === null) {
    dtEl.textContent = "—";
    freqEl.textContent = "—";
    output.textContent = `A = ${cursorA.toFixed(4)} s. Click for B.`;
    tbody.replaceChildren(
      ...visibleSeries.map((item) => {
        const tr = document.createElement("tr");
        const va = valueAt(item, cursorA!);
        tr.innerHTML = `<td title="${item.name}">${item.name}</td><td>${formatNumber(va, 5)}</td><td>—</td><td>—</td><td>—</td>`;
        return tr;
      }),
    );
    return;
  }

  const dt = cursorB - cursorA;
  dtEl.textContent = formatSeconds(dt);
  freqEl.textContent = formatFrequency(dt);

  const first = visibleSeries[0];
  let text = `Δt = ${formatSeconds(dt)} (${formatFrequency(dt)})`;
  if (first) {
    const a = valueAt(first, cursorA);
    const b = valueAt(first, cursorB);
    if (a !== null && b !== null) {
      text += `  Δ${first.name} = ${(b - a).toPrecision(5)}`;
    }
  }
  output.textContent = text;

  tbody.replaceChildren(
    ...visibleSeries.map((item) => {
      const tr = document.createElement("tr");
      const va = valueAt(item, cursorA!);
      const vb = valueAt(item, cursorB!);
      const dv = va !== null && vb !== null ? vb - va : null;
      const slope = dv !== null && Math.abs(dt) > 1e-12 ? dv / dt : null;
      tr.innerHTML = `<td title="${item.name}">${item.name}</td><td>${formatNumber(va, 5)}</td><td>${formatNumber(vb, 5)}</td><td>${formatNumber(dv, 5)}</td><td>${slope !== null ? `${formatNumber(slope, 4)}/s` : "—"}</td>`;
      return tr;
    }),
  );
}

function isSeriesVisible(item: SeriesInfo): boolean {
  if (soloSeriesId !== null) {
    return item.seriesId === soloSeriesId;
  }
  return !hidden.has(item.seriesId);
}

function destroyPlots(): void {
  for (const group of groups) {
    group.plot.destroy();
    group.element.remove();
  }
  groups = [];
}

function computePlotHeight(totalGroups: number, collapsed: boolean): number {
  if (collapsed) return 0;
  const container = byId("plots");
  const available = Math.max(200, container.clientHeight - 16);
  const headerOverhead = 32;
  return Math.max(130, Math.floor(available / Math.max(1, totalGroups)) - headerOverhead);
}

function buildGroup(name: string, members: SeriesInfo[], total: number): GroupPlot {
  const yCfg = getGroupYScale(name);
  const element = document.createElement("div");
  element.className = "group" + (yCfg.collapsed ? " collapsed" : "");

  // Group Header Bar
  const header = document.createElement("div");
  header.className = "group-header";

  const title = document.createElement("h2");
  title.textContent = name;

  const legend = document.createElement("div");
  legend.className = "group-legend";
  const legendValues = new Map<number, HTMLElement>();

  members.forEach((item) => {
    const color = colorOf(item, series.indexOf(item));
    const pill = document.createElement("span");
    pill.className = "legend-pill";
    const dot = document.createElement("span");
    dot.className = "legend-dot";
    dot.style.background = color;
    const lbl = document.createElement("span");
    lbl.className = "legend-name";
    lbl.textContent = item.name;
    const val = document.createElement("span");
    val.className = "legend-val";
    val.textContent = "—";
    legendValues.set(item.seriesId, val);
    pill.append(dot, lbl, val);
    legend.append(pill);
  });

  // Y-axis controls
  const controls = document.createElement("div");
  controls.className = "group-controls";

  const yRangeWrap = document.createElement("span");
  yRangeWrap.className = "y-range-inputs";
  yRangeWrap.hidden = yCfg.auto;

  const yMinInput = document.createElement("input");
  yMinInput.type = "number";
  yMinInput.step = "any";
  yMinInput.title = "Y-axis minimum";
  yMinInput.value = formatNumber(yCfg.min, 4);

  const ySep = document.createElement("span");
  ySep.textContent = "..";

  const yMaxInput = document.createElement("input");
  yMaxInput.type = "number";
  yMaxInput.step = "any";
  yMaxInput.title = "Y-axis maximum";
  yMaxInput.value = formatNumber(yCfg.max, 4);

  yRangeWrap.append(yMinInput, ySep, yMaxInput);

  const yAutoBtn = document.createElement("button");
  yAutoBtn.type = "button";
  yAutoBtn.className = "mini-btn";
  yAutoBtn.title = "Toggle automatic Y-axis scaling vs manual/locked Y range (or Shift+Wheel on plot)";
  yAutoBtn.textContent = yCfg.auto ? "Auto Y" : "Locked Y";
  yAutoBtn.setAttribute("aria-pressed", String(!yCfg.auto));

  const collapseBtn = document.createElement("button");
  collapseBtn.type = "button";
  collapseBtn.className = "mini-btn";
  collapseBtn.title = "Collapse/expand subplot";
  collapseBtn.textContent = yCfg.collapsed ? "▸" : "▾";

  controls.append(yRangeWrap, yAutoBtn, collapseBtn);
  header.append(title, legend, controls);

  const canvasWrap = document.createElement("div");
  canvasWrap.className = "group-canvas-wrap";

  element.append(header, canvasWrap);
  byId("plots").append(element);

  const definitions: uPlot.Series[] = [{}];
  const bands: uPlot.Band[] = [];
  members.forEach((item) => {
    const color = colorOf(item, series.indexOf(item));
    const base = { stroke: color, width: 1.5, points: { show: false }, spanGaps: false } as const;
    definitions.push(
      {
        ...base,
        label: `${item.name} max`,
        points: { show: () => division <= MARKERS_AT_DIVISION, size: 5, fill: color, stroke: color },
      },
      { ...base, label: `${item.name} min`, width: showBands ? 1 : 1.5 },
    );
    if (showBands) {
      const index = definitions.length - 2;
      bands.push({ series: [index, index + 1], fill: withAlpha(color, canvasTheme.isLight ? "26" : "33") });
    }
  });

  const height = computePlotHeight(total, yCfg.collapsed);
  const plot = new uPlot(
    {
      width: canvasWrap.clientWidth || byId("plots").clientWidth - 18 || 600,
      height: Math.max(80, height),
      legend: { show: false },
      cursor: {
        drag: { x: false, y: false, setScale: false },
        sync: { key: "timeline" },
      },
      scales: {
        x: { time: false, auto: false },
        y: {
          auto: () => getGroupYScale(name).auto,
          range: (_u, dataMin, dataMax) => {
            const cfg = getGroupYScale(name);
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
          stroke: canvasTheme.axisStroke,
          grid: { stroke: canvasTheme.gridMajor },
          ticks: { stroke: canvasTheme.tickStroke },
          splits: divisionSplits,
          values: divisionLabels,
          font: "11px ui-monospace, monospace",
        },
        {
          stroke: canvasTheme.axisStroke,
          grid: { stroke: canvasTheme.gridMinor },
          ticks: { stroke: canvasTheme.tickStroke },
          size: 66,
          font: "11px ui-monospace, monospace",
        },
      ],
      series: definitions,
      bands,
      hooks: { draw: [drawOverlays(name, members)] },
    },
    [[], ...members.flatMap(() => [[], []])] as unknown as uPlot.AlignedData,
    canvasWrap,
  );

  const groupObj: GroupPlot = {
    name,
    members,
    plot,
    element,
    canvasWrap,
    legendValues,
    yAutoBtn,
    yRangeWrap,
    yMinInput,
    yMaxInput,
  };

  yAutoBtn.onclick = () => {
    const cfg = getGroupYScale(name);
    if (cfg.auto) {
      cfg.auto = false;
      cfg.min = plot.scales.y.min ?? -1;
      cfg.max = plot.scales.y.max ?? 1;
    } else {
      cfg.auto = true;
    }
    syncGroupYControls(groupObj);
    if (lastColumns) draw(lastColumns.result);
  };

  const onManualYChange = () => {
    const minV = Number(yMinInput.value);
    const maxV = Number(yMaxInput.value);
    if (Number.isFinite(minV) && Number.isFinite(maxV) && maxV > minV) {
      const cfg = getGroupYScale(name);
      cfg.auto = false;
      cfg.min = minV;
      cfg.max = maxV;
      applyGroupYScale(groupObj);
    }
  };
  yMinInput.onchange = onManualYChange;
  yMaxInput.onchange = onManualYChange;

  collapseBtn.onclick = () => {
    const cfg = getGroupYScale(name);
    cfg.collapsed = !cfg.collapsed;
    element.classList.toggle("collapsed", cfg.collapsed);
    collapseBtn.textContent = cfg.collapsed ? "▸" : "▾";
    resizeAllPlots();
  };

  attachInteractions(groupObj);
  return groupObj;
}

function effectiveGroupOf(item: SeriesInfo): string {
  if (splitLanes) return item.name;
  return item.group && item.group.trim() ? item.group.trim() : DEFAULT_GROUP;
}

function rebuild(): void {
  canvasTheme = themeManager.getCanvasTheme();
  destroyPlots();
  if (soloSeriesId !== null && !series.some((s) => s.seriesId === soloSeriesId)) {
    soloSeriesId = null;
  }
  const visible = series.filter((item) => isSeriesVisible(item) && item.actualMode !== "none");
  byId("empty").hidden = series.length > 0;
  const names = [...new Set(visible.map((item) => effectiveGroupOf(item)))];
  for (const name of names) {
    const members = visible.filter((item) => effectiveGroupOf(item) === name);
    groups.push(buildGroup(name, members, names.length));
  }
  renderSidebar();
  updateSuggestions();
  if (lastColumns) {
    draw(lastColumns.result);
  }
  updateMeasure();
}

function describeMode(item: SeriesInfo): string {
  if (!item.rateHz) return item.actualMode;
  const rate = item.rateHz >= 1000 ? `${item.rateHz / 1000} kHz` : `${item.rateHz} Hz`;
  return `${item.actualMode} ${rate}`;
}

function renderSidebar(): void {
  const list = byId("series");
  list.replaceChildren();
  signalDomRefs.clear();

  byId("series-count").textContent = String(series.length);
  const filter = seriesFilter.trim().toLowerCase();

  series.forEach((item, index) => {
    if (filter) {
      const hay = `${item.name} ${item.root} ${item.group ?? ""} ${item.actualMode}`.toLowerCase();
      if (!hay.includes(filter)) return;
    }

    const color = colorOf(item, index);
    const visible = isSeriesVisible(item);

    const card = document.createElement("div");
    card.className = "series" + (item.error ? " broken" : "") + (!visible ? " is-hidden" : "");
    card.style.borderLeftColor = color;
    card.title = item.error ? `Error: ${item.error}\nRoot: ${item.root}` : `Expression: ${item.root}`;

    // Top row: checkbox + color picker + signal name + mode badge + Solo + Remove
    const topRow = document.createElement("div");
    topRow.className = "series-top";

    const toggle = document.createElement("input");
    toggle.type = "checkbox";
    toggle.checked = visible;
    toggle.style.accentColor = color;
    toggle.title = "Show/hide signal on Timeline";
    toggle.onchange = () => {
      soloSeriesId = null;
      if (toggle.checked) hidden.delete(item.seriesId);
      else hidden.add(item.seriesId);
      rebuild();
    };

    const colorPicker = document.createElement("input");
    colorPicker.type = "color";
    colorPicker.className = "series-color-picker";
    colorPicker.value = toHexColor(color);
    colorPicker.title = "Change signal color";
    colorPicker.onchange = async () => {
      const newColor = colorPicker.value;
      item.color = newColor;
      try {
        await client.call("timeline.update", {
          seriesId: item.seriesId,
          color: newColor,
          ...(item.group ? { group: item.group } : {}),
        });
      } catch {
        // Ignore if not supported on RTT series
      }
      rebuild();
    };

    const name = document.createElement("span");
    name.className = "name";
    name.textContent = item.name;

    const mode = document.createElement("span");
    mode.className = `mode ${item.actualMode}`;
    mode.textContent = item.error ? "error" : describeMode(item);

    const actions = document.createElement("div");
    actions.className = "series-actions";

    const soloBtn = document.createElement("button");
    soloBtn.type = "button";
    soloBtn.className = "solo-btn" + (soloSeriesId === item.seriesId ? " active" : "");
    soloBtn.textContent = "S";
    soloBtn.title = "Solo this signal (isolate)";
    soloBtn.onclick = (event) => {
      event.preventDefault();
      soloSeriesId = soloSeriesId === item.seriesId ? null : item.seriesId;
      rebuild();
    };

    const remove = document.createElement("button");
    remove.type = "button";
    remove.className = "remove-btn";
    remove.textContent = "×";
    remove.title = "Remove signal";
    remove.onclick = async (event) => {
      event.preventDefault();
      if (soloSeriesId === item.seriesId) soloSeriesId = null;
      await client.call("timeline.remove", { seriesId: item.seriesId });
      await loadSeries();
    };

    actions.append(soloBtn, remove);
    topRow.append(toggle, colorPicker, name, mode, actions);

    // Middle row: Editable group tag + Live value readout
    const metaRow = document.createElement("div");
    metaRow.className = "series-meta-row";

    const groupInput = document.createElement("input");
    groupInput.type = "text";
    groupInput.className = "series-group-input";
    groupInput.placeholder = "default";
    groupInput.value = item.group ?? "";
    groupInput.title = "Subplot group name (press Enter or blur to move signal to another subplot)";
    groupInput.setAttribute("list", "group-suggestions");
    const commitGroup = async () => {
      const nextGroup = groupInput.value.trim() || null;
      if (nextGroup === (item.group ?? null)) return;
      item.group = nextGroup;
      try {
        await client.call("timeline.update", {
          seriesId: item.seriesId,
          ...(nextGroup ? { group: nextGroup } : {}),
          ...(item.color ? { color: item.color } : {}),
        });
      } catch {
        // Fallback local group update
      }
      rebuild();
    };
    groupInput.onchange = () => void commitGroup();

    const liveVal = document.createElement("span");
    liveVal.className = "series-live-val";
    liveVal.textContent = "—";
    metaRow.append(groupInput, liveVal);

    // Bottom row: Min / Max / Avg / Peak-to-Peak statistics
    const statsRow = document.createElement("div");
    statsRow.className = "series-stats-row";

    const makeStat = (label: string) => {
      const cell = document.createElement("div");
      cell.className = "stat-cell";
      const lbl = document.createElement("span");
      lbl.textContent = label;
      const val = document.createElement("span");
      val.textContent = "—";
      cell.append(lbl, val);
      statsRow.append(cell);
      return val;
    };

    const minVal = makeStat("MIN");
    const maxVal = makeStat("MAX");
    const avgVal = makeStat("AVG");
    const ppVal = makeStat("Δ P-P");

    signalDomRefs.set(item.seriesId, { liveVal, minVal, maxVal, avgVal, ppVal });

    card.append(topRow, metaRow, statsRow);
    list.append(card);
  });

  updateHoverReadouts();
}

function renderRttSignals(): void {
  const drawer = byId("rtt-drawer");
  const countEl = byId("rtt-signal-count");
  const listEl = byId("rtt-signal-list");

  if (rttSignals.length === 0) {
    drawer.hidden = true;
    return;
  }
  drawer.hidden = false;
  countEl.textContent = String(rttSignals.length);
  listEl.replaceChildren(
    ...rttSignals.map((sig) => {
      const row = document.createElement("div");
      row.className = "rtt-sig-row";
      const label = document.createElement("span");
      label.className = "mono";
      label.textContent = `${sig.name}${sig.value !== null ? ` (${formatNumber(sig.value, 4)})` : ""}`;
      const btn = document.createElement("button");
      btn.type = "button";
      btn.className = "mini-btn";
      btn.textContent = sig.seriesId === null ? "+ Plot" : "Remove";
      btn.onclick = async () => {
        if (sig.seriesId === null) {
          await client.call("timeline.add", { rttSignal: sig.name });
        } else {
          await client.call("timeline.remove", { seriesId: sig.seriesId });
        }
        await loadSeries();
      };
      row.append(label, btn);
      return row;
    }),
  );
}

async function loadSeries(): Promise<void> {
  const reply = await client.call<{ series: SeriesInfo[] }>("timeline.list");
  series = reply.series;
  try {
    const rttReply = await client.call<{ signals: RttSignalInfo[] }>("rtt.signals");
    rttSignals = rttReply.signals ?? [];
    renderRttSignals();
  } catch {
    // RTT signals optional
  }
  rebuild();
}

/** Columns inside a halt are real gaps. Elsewhere an empty column only means
 *  samples are sparser than pixels, so the line continues across it. */
function isHalted(seconds: number): boolean {
  for (let index = halts.length - 1; index >= 0; index--) {
    const halt = halts[index];
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

function drawOverview(): void {
  const canvas = byId<HTMLCanvasElement>("overview-canvas");
  const rect = canvas.getBoundingClientRect();
  if (rect.width <= 0 || rect.height <= 0) return;
  const dpr = devicePixelRatio || 1;
  const w = Math.round(rect.width * dpr);
  const h = Math.round(rect.height * dpr);
  if (canvas.width !== w || canvas.height !== h) {
    canvas.width = w;
    canvas.height = h;
  }
  const ctx = canvas.getContext("2d");
  if (!ctx) return;

  ctx.clearRect(0, 0, w, h);

  const now = Math.max(nowSeconds(), viewEnd, 1);
  const start = Math.min(historyStart, Math.max(0, viewEnd - spanSeconds()));
  const totalSpan = Math.max(1, now - start);
  const toX = (t: number) => ((t - start) / totalSpan) * w;

  // Draw halt intervals
  ctx.fillStyle = canvasTheme.haltFill;
  for (const halt of halts) {
    const x0 = Math.max(0, toX(halt.from));
    const x1 = Math.min(w, toX(halt.to ?? now));
    if (x1 > x0) ctx.fillRect(x0, 0, x1 - x0, h);
  }

  // Draw event ticks
  for (const m of markers) {
    const mx = toX(m.t);
    if (mx >= 0 && mx <= w) {
      ctx.fillStyle = eventKindName(m.kind).color();
      ctx.fillRect(mx - dpr * 0.5, 0, Math.max(1.5, dpr), h);
    }
  }

  // Draw cursors A & B
  if (cursorA !== null) {
    const ax = toX(cursorA);
    ctx.fillStyle = canvasTheme.cursorA;
    ctx.fillRect(ax - dpr * 0.5, 0, Math.max(1.5, dpr), h);
  }
  if (cursorB !== null) {
    const bx = toX(cursorB);
    ctx.fillStyle = canvasTheme.cursorB;
    ctx.fillRect(bx - dpr * 0.5, 0, Math.max(1.5, dpr), h);
  }

  // Draw active viewport window
  const vEnd = followLive ? nowSeconds() - FOLLOW_LAG_S : viewEnd;
  const vStart = vEnd - spanSeconds();
  const vx0 = Math.max(0, Math.min(w - 4, toX(vStart)));
  const vx1 = Math.max(vx0 + 4, Math.min(w, toX(vEnd)));

  ctx.fillStyle = canvasTheme.selectionFill;
  ctx.fillRect(vx0, 0, vx1 - vx0, h);
  ctx.strokeStyle = canvasTheme.selectionStroke;
  ctx.lineWidth = Math.max(1, dpr);
  ctx.strokeRect(vx0 + 0.5, 0.5, Math.max(2, vx1 - vx0 - 1), h - 1);

  byId("overview-range").textContent = `${Math.max(0, vStart).toFixed(2)} s — ${Math.max(0, vEnd).toFixed(2)} s`;
}

function draw(result: QueryResult): void {
  const step = (result.t1 - result.t0) / result.columns / NS;
  const start = result.t0 / NS;
  const end = result.t1 / NS;
  if (start > 0 && (historyStart === 0 || start < historyStart)) {
    historyStart = start;
  }
  const xs = Array.from({ length: result.columns }, (_, index) => start + index * step);
  const gaps = xs.map(isHalted);
  lastColumns = { result, xs };
  byId("plots").dataset.range = `${result.t0},${result.t1}`;

  for (const group of groups) {
    const arrays: (number | null | undefined)[][] = [];
    for (const member of group.members) {
      const data = result.series.get(member.seriesId);
      if (data) {
        arrays.push(toPlotValues(data.max, gaps), toPlotValues(data.min, gaps));
      } else {
        const empty = new Array<null>(result.columns).fill(null);
        arrays.push(empty, empty);
      }
    }
    group.plot.batch(() => {
      group.plot.setData([xs, ...arrays] as unknown as uPlot.AlignedData, false);
      group.plot.setScale("x", { min: start, max: end });
      const yCfg = getGroupYScale(group.name);
      if (!yCfg.auto && yCfg.max > yCfg.min) {
        group.plot.setScale("y", { min: yCfg.min, max: yCfg.max });
      }
    });
  }

  updateHoverReadouts();
  if (cursorA !== null) {
    updateMeasure();
  }
  drawOverview();
}

async function refresh(): Promise<void> {
  querying = true;
  try {
    const ids = groups.flatMap((group) => group.members.map((member) => member.seriesId));
    const width = groups[0]?.plot.bbox.width ? Math.round(groups[0].plot.bbox.width / devicePixelRatio) : 0;
    if (ids.length > 0 && width > 0) {
      const end = followLive ? nowSeconds() - FOLLOW_LAG_S : viewEnd;
      if (followLive) viewEnd = end;
      const result = await client.call<QueryResult>("timeline.query", {
        seriesIds: ids,
        t0: Math.round((end - spanSeconds()) * NS),
        t1: Math.round(end * NS),
        widthPx: width,
      });
      draw(result);
    } else {
      drawOverview();
    }
  } catch {
    // Reconnecting; the next frame tries again.
  } finally {
    querying = false;
  }
}

function frame(time: number): void {
  if (!querying && time - lastQuery >= REFRESH_MS) {
    lastQuery = time;
    void refresh();
  }
  requestAnimationFrame(frame);
}

interface Stats {
  hss: { records: number; lost: number; bytes?: number };
  poll?: { cycles: number; errors: number };
}

function setConnectionStatus(text: string, state: "connected" | "disconnected" | "reconnecting"): void {
  const conn = byId("connection");
  conn.innerHTML = `<span class="status-dot ${state === "connected" ? "connected" : state === "disconnected" ? "disconnected" : ""}"></span>`;
  conn.append(document.createTextNode(text));
}

function syncRecordButton(active: boolean): void {
  recording = active;
  const button = byId<HTMLButtonElement>("record");
  button.textContent = recording ? "● Stop Rec" : "Record";
  button.classList.toggle("active", recording);
}

async function refreshStatus(): Promise<void> {
  try {
    const status = await client.call<{
      nowNs: number;
      connected: boolean;
      device?: string;
      speedKhz?: number;
      recording?: string | null;
    }>("session.status");
    const estimate = nowSeconds();
    const reported = status.nowNs / NS;
    if (Math.abs(estimate - reported) > 0.02 || serverNow === 0) {
      serverNow = reported;
      serverNowAt = performance.now();
    }
    if (typeof status.recording === "string") {
      syncRecordButton(true);
    }
    const stats = await client.call<Stats>("session.stats");
    if (status.connected) {
      setConnectionStatus(`${status.device} @ ${(status.speedKhz ?? 0) / 1000} MHz`, "connected");
    } else {
      setConnectionStatus("No target connected", "disconnected");
    }

    const hssCount = series.filter((item) => item.actualMode === "hss").length;
    const pollCount = series.filter((item) => item.actualMode === "poll").length;
    const rate = series.find((item) => item.actualMode === "hss")?.rateHz ?? 0;
    const parts: string[] = [];
    if (hssCount > 0) {
      parts.push(`HSS ${hssCount} sig @ ${rate} Hz (lost ${stats.hss.lost})`);
    }
    if (pollCount > 0) {
      const pRate = series.find((item) => item.actualMode === "poll")?.rateHz ?? 0;
      parts.push(`Poll ${pollCount} sig @ ${pRate} Hz`);
    }
    byId("rates").textContent = parts.join(" │ ");
    syncFollowButton();
  } catch {
    setConnectionStatus("Reconnecting…", "reconnecting");
  }
}

interface Rate {
  rateHz: number;
  maxHz: number;
}

function showRate(rate: Rate): void {
  const select = byId<HTMLSelectElement>("rate");
  const value = String(rate.rateHz);
  if (![...select.options].some((option) => option.value === value)) {
    select.append(new Option(`${value} Hz`, value));
  }
  select.value = value;
  select.classList.toggle("off", rate.rateHz === 0);
}

async function changeRate(rateHz: number): Promise<void> {
  try {
    showRate(await client.call<Rate>("timeline.setRate", { rateHz }));
    await loadSeries();
  } catch (error) {
    byId("measure").textContent = String(error);
    showRate(await client.call<Rate>("timeline.rate"));
  }
}

async function start(): Promise<void> {
  const reply = await client.call<{ events: TimelineEvent[] }>("events.query", {});
  halts = [];
  markers = [];
  allEvents = [];
  reply.events.forEach(applyEvent);
  await refreshStatus();
  showRate(await client.call<Rate>("timeline.rate"));
  await loadSeries();
}

function resizeAllPlots(): void {
  const visibleGroups = groups.filter((g) => !getGroupYScale(g.name).collapsed);
  for (const group of groups) {
    const cfg = getGroupYScale(group.name);
    if (cfg.collapsed) continue;
    const width = Math.max(200, group.canvasWrap.clientWidth || byId("plots").clientWidth - 18);
    const height = computePlotHeight(visibleGroups.length, false);
    group.plot.setSize({ width, height });
  }
  drawOverview();
}

function exportPngSnapshot(): void {
  const activeGroups = groups.filter((g) => !getGroupYScale(g.name).collapsed);
  if (activeGroups.length === 0) {
    byId("measure").textContent = "Nothing to export: plot at least one signal first.";
    return;
  }
  const dpr = devicePixelRatio || 1;
  const pad = Math.round(16 * dpr);
  const headerH = Math.round(36 * dpr);
  const groupTitleH = Math.round(22 * dpr);
  const width = Math.max(...activeGroups.map((g) => g.plot.ctx.canvas.width)) + pad * 2;
  const plotsH = activeGroups.reduce((sum, g) => sum + g.plot.ctx.canvas.height + groupTitleH + Math.round(8 * dpr), 0);
  const height = headerH + plotsH + pad * 2;

  const out = document.createElement("canvas");
  out.width = width;
  out.height = height;
  const ctx = out.getContext("2d");
  if (!ctx) return;

  ctx.fillStyle = canvasTheme.bg;
  ctx.fillRect(0, 0, width, height);

  ctx.fillStyle = canvasTheme.axisStroke;
  ctx.font = `bold ${Math.round(13 * dpr)}px system-ui, sans-serif`;
  const statusText = byId("connection").textContent ?? "jlinkd Timeline";
  const measureText = byId("measure").textContent ?? "";
  ctx.fillText(`jlinkd Timeline — ${statusText}   ${measureText}`, pad, pad + Math.round(16 * dpr));

  let y = pad + headerH;
  for (const g of activeGroups) {
    ctx.fillStyle = canvasTheme.axisStroke;
    ctx.font = `600 ${Math.round(11 * dpr)}px ui-monospace, monospace`;
    const memberSummary = g.members.map((m) => m.name).join(", ");
    ctx.fillText(`${g.name}: ${memberSummary}`, pad, y + Math.round(14 * dpr));
    y += groupTitleH;
    ctx.drawImage(g.plot.ctx.canvas, pad, y);
    y += g.plot.ctx.canvas.height + Math.round(8 * dpr);
  }

  const link = document.createElement("a");
  link.download = `jlinkd-timeline-${Date.now()}.png`;
  link.href = out.toDataURL("image/png");
  link.click();
  byId("measure").textContent = `Saved PNG snapshot (${link.download})`;
}

// ============================================================================
// Event Listeners & UI Wiring
// ============================================================================

client.onEvent = (event) => {
  applyEvent(event);
  if (event.kind !== EVENT_HALTED && event.kind !== EVENT_RESUMED) {
    void loadSeries();
  }
};
client.onConnection = (connected) => {
  if (connected) {
    void start();
  } else {
    setConnectionStatus("Reconnecting…", "reconnecting");
  }
};

// Theme synchronization (VS Code + manual selector)
const themeSelect = byId<HTMLSelectElement>("theme-select");
themeSelect.value = themeManager.getPreference();
themeSelect.addEventListener("change", () => {
  themeManager.setPreference(themeSelect.value as ThemePreference);
});
themeManager.onChange((newCanvasTheme) => {
  canvasTheme = newCanvasTheme;
  themeSelect.value = themeManager.getPreference();
  rebuild();
});

// Add signal form
byId<HTMLSelectElement>("mode").addEventListener("change", updateSuggestions);
byId<HTMLFormElement>("add").addEventListener("submit", async (event) => {
  event.preventDefault();
  const input = byId<HTMLInputElement>("expr");
  const expr = input.value.trim();
  if (!expr) return;
  const mode = byId<HTMLSelectElement>("mode").value;
  const group = byId<HTMLInputElement>("group").value.trim();
  try {
    if (mode === "rtt") {
      await client.call("timeline.add", {
        rttSignal: expr,
        ...(group ? { group } : {}),
      });
    } else {
      await client.call("timeline.add", {
        expr,
        mode,
        ...(group ? { group } : {}),
      });
      rememberExpr(expr);
    }
    input.value = "";
    await loadSeries();
  } catch (error) {
    byId("measure").textContent = String(error);
  }
});

// Rate & Timebase controls
byId<HTMLSelectElement>("rate").addEventListener("change", (event) => {
  void changeRate(Number((event.target as HTMLSelectElement).value));
});
byId<HTMLSelectElement>("division").addEventListener("change", (event) => {
  division = Number((event.target as HTMLSelectElement).value);
  updateDivisionSelect();
  void refresh();
});
byId("zoom-in").addEventListener("click", () => {
  stepDivision(-1);
  void refresh();
});
byId("zoom-out").addEventListener("click", () => {
  stepDivision(1);
  void refresh();
});

// Navigation & Target controls
byId("pan-left").addEventListener("click", () => {
  stopFollowing();
  viewEnd -= spanSeconds() * 0.25;
  syncFollowButton();
  void refresh();
});
byId("pan-right").addEventListener("click", () => {
  stopFollowing();
  viewEnd = Math.min(nowSeconds(), viewEnd + spanSeconds() * 0.25);
  syncFollowButton();
  void refresh();
});
byId("follow").addEventListener("click", () => {
  followLive = !followLive;
  if (followLive) {
    viewEnd = nowSeconds() - FOLLOW_LAG_S;
  }
  syncFollowButton();
  void refresh();
});
byId("target-halt").addEventListener("click", async () => {
  try {
    await client.call("target.halt");
  } catch (error) {
    byId("measure").textContent = String(error);
  }
});
byId("target-go").addEventListener("click", async () => {
  try {
    await client.call("target.go");
  } catch (error) {
    byId("measure").textContent = String(error);
  }
});
byId("target-reset").addEventListener("click", async () => {
  try {
    await client.call("target.reset");
  } catch (error) {
    byId("measure").textContent = String(error);
  }
});

// Layout, Band, Record, Export, Snapshot
byId("layout-mode").addEventListener("click", () => {
  splitLanes = !splitLanes;
  const btn = byId<HTMLButtonElement>("layout-mode");
  btn.textContent = splitLanes ? "Split Lanes" : "Grouped";
  btn.setAttribute("aria-pressed", String(splitLanes));
  rebuild();
});
byId("toggle-band").addEventListener("click", () => {
  showBands = !showBands;
  const btn = byId<HTMLButtonElement>("toggle-band");
  btn.setAttribute("aria-pressed", String(showBands));
  rebuild();
});
byId("record").addEventListener("click", async () => {
  try {
    if (recording) {
      const result = await client.call<{ path: string; samples: number }>("record.stop");
      byId("measure").textContent = `Saved ${result.samples} samples to ${result.path}`;
      syncRecordButton(false);
    } else {
      await client.call("record.start");
      syncRecordButton(true);
    }
  } catch (error) {
    byId("measure").textContent = String(error);
  }
});
byId("export").addEventListener("click", async () => {
  const end = followLive ? nowSeconds() : viewEnd;
  try {
    const result = await client.call<{ path: string; rows: number }>("record.export", {
      format: "csv",
      seriesIds: series.map((item) => item.seriesId),
      t0: Math.round((end - spanSeconds()) * NS),
      t1: Math.round(end * NS),
    });
    byId("measure").textContent = `Wrote ${result.rows} rows to ${result.path}`;
  } catch (error) {
    byId("measure").textContent = String(error);
  }
});
byId("snapshot").addEventListener("click", exportPngSnapshot);

// Sidebar controls & Resizer
byId("toggle-sidebar").addEventListener("click", () => {
  const sb = byId("sidebar");
  sb.classList.toggle("collapsed");
  resizeAllPlots();
});
byId<HTMLInputElement>("series-filter").addEventListener("input", (event) => {
  seriesFilter = (event.target as HTMLInputElement).value;
  renderSidebar();
});
byId("btn-show-all").addEventListener("click", () => {
  soloSeriesId = null;
  hidden.clear();
  rebuild();
});
byId("btn-hide-all").addEventListener("click", () => {
  soloSeriesId = null;
  series.forEach((s) => hidden.add(s.seriesId));
  rebuild();
});
byId("btn-clear-all").addEventListener("click", async () => {
  soloSeriesId = null;
  hidden.clear();
  await Promise.all(series.map((s) => client.call("timeline.remove", { seriesId: s.seriesId })));
  await loadSeries();
});

// Cursor panel buttons
byId("cursor-clear").addEventListener("click", clearCursors);
byId("cursor-zoom").addEventListener("click", () => {
  if (cursorA !== null && cursorB !== null) {
    zoomToRange(cursorA, cursorB);
  }
});

// Overview scrubber interaction
(() => {
  const track = byId<HTMLCanvasElement>("overview-canvas");
  let scrubbing = false;
  const scrubTo = (clientX: number) => {
    const rect = track.getBoundingClientRect();
    if (rect.width <= 0) return;
    const frac = Math.max(0, Math.min(1, (clientX - rect.left) / rect.width));
    const now = Math.max(nowSeconds(), viewEnd, 1);
    const start = Math.min(historyStart, Math.max(0, viewEnd - spanSeconds()));
    const center = start + frac * (now - start);
    stopFollowing();
    viewEnd = center + spanSeconds() * 0.5;
    syncFollowButton();
    void refresh();
  };
  track.addEventListener("mousedown", (e) => {
    scrubbing = true;
    scrubTo(e.clientX);
  });
  window.addEventListener("mousemove", (e) => {
    if (scrubbing) scrubTo(e.clientX);
  });
  window.addEventListener("mouseup", () => {
    scrubbing = false;
  });
})();

// Sidebar resizer
(() => {
  const sidebar = byId("sidebar");
  const resizer = byId("sidebar-resizer");
  try {
    const savedW = Number(localStorage.getItem(SIDEBAR_WIDTH_KEY));
    if (savedW >= 190 && savedW <= 520) {
      sidebar.style.width = `${savedW}px`;
    }
  } catch {
    // Ignore storage errors
  }
  let startX = 0;
  let startW = 280;
  let resizing = false;
  resizer.addEventListener("mousedown", (e) => {
    resizing = true;
    startX = e.clientX;
    startW = sidebar.getBoundingClientRect().width;
    resizer.classList.add("dragging");
  });
  window.addEventListener("mousemove", (e) => {
    if (!resizing) return;
    const w = Math.max(190, Math.min(520, startW + (e.clientX - startX)));
    sidebar.style.width = `${w}px`;
    resizeAllPlots();
  });
  window.addEventListener("mouseup", () => {
    if (!resizing) return;
    resizing = false;
    resizer.classList.remove("dragging");
    try {
      localStorage.setItem(SIDEBAR_WIDTH_KEY, String(Math.round(sidebar.getBoundingClientRect().width)));
    } catch {
      // Ignore storage errors
    }
  });
})();

// Events popover & Help modal
byId("events-btn").addEventListener("click", () => {
  const pop = byId("events-popover");
  pop.hidden = !pop.hidden;
});
byId("events-close").addEventListener("click", () => {
  byId("events-popover").hidden = true;
});
byId("help-btn").addEventListener("click", () => {
  byId("help-modal").hidden = false;
});
byId("help-close").addEventListener("click", () => {
  byId("help-modal").hidden = true;
});
byId("help-modal").addEventListener("click", (e) => {
  if (e.target === byId("help-modal")) {
    byId("help-modal").hidden = true;
  }
});

// Global Keyboard Shortcuts
window.addEventListener("keydown", (event) => {
  const tag = (event.target as HTMLElement | null)?.tagName;
  if (tag === "INPUT" || tag === "SELECT" || tag === "TEXTAREA") {
    if (event.key === "Escape") {
      (event.target as HTMLElement).blur();
    }
    return;
  }

  switch (event.key) {
    case " ":
      event.preventDefault();
      byId("follow").click();
      break;
    case "ArrowLeft":
      event.preventDefault();
      byId("pan-left").click();
      break;
    case "ArrowRight":
      event.preventDefault();
      byId("pan-right").click();
      break;
    case "-":
    case "_":
      event.preventDefault();
      byId("zoom-in").click();
      break;
    case "=":
    case "+":
      event.preventDefault();
      byId("zoom-out").click();
      break;
    case "a":
    case "A":
      if (hoverTime !== null) {
        cursorA = hoverTime;
        updateMeasure();
        redrawAllOverlays();
      }
      break;
    case "b":
    case "B":
      if (hoverTime !== null) {
        cursorB = hoverTime;
        updateMeasure();
        redrawAllOverlays();
      } else {
        byId("toggle-sidebar").click();
      }
      break;
    case "Escape":
      if (!byId("help-modal").hidden) {
        byId("help-modal").hidden = true;
      } else if (!byId("events-popover").hidden) {
        byId("events-popover").hidden = true;
      } else {
        clearCursors();
      }
      break;
    case "/":
      event.preventDefault();
      byId<HTMLInputElement>("expr").focus();
      break;
    case "?":
      byId("help-modal").hidden = !byId("help-modal").hidden;
      break;
  }
});

new ResizeObserver(() => {
  resizeAllPlots();
}).observe(byId("plots"));

updateDivisionSelect();
updateSuggestions();
client.connect();
setInterval(() => void refreshStatus(), STATUS_MS);
requestAnimationFrame(frame);
