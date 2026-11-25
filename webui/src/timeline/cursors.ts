import {
  EVENT_FAULT,
  EVENT_HALTED,
  EVENT_RELOAD,
  EVENT_RESET,
  EVENT_RESUMED,
  EVENT_SAMPLING,
  type SeriesInfo,
} from "../client";
import {
  divisionLabels,
  divisionSplits,
  formatFrequency,
  formatNumber,
  formatSeconds,
  pxToTime,
  spanSeconds,
  timeToPx,
} from "./math";
import {
  colorOf,
  isSeriesVisible,
  nowSeconds,
  valueAt,
  type TimelineState,
  type TrackRow,
} from "./state";

const monoCharWidthCache = new Map<string, number>();

function measureMonoWidth(ctx: CanvasRenderingContext2D, font: string, text: string): number {
  let asciiOnly = true;
  for (let i = 0; i < text.length; i++) {
    if (text.charCodeAt(i) > 127) {
      asciiOnly = false;
      break;
    }
  }
  if (!asciiOnly) {
    return ctx.measureText(text).width;
  }
  let charW = monoCharWidthCache.get(font);
  if (charW === undefined) {
    charW = ctx.measureText("0000000000").width / 10;
    monoCharWidthCache.set(font, charW);
  }
  return charW * text.length;
}

export function eventKindInfo(
  state: TimelineState,
  kind: number,
): { label: string; cls: string; color: string } {
  const t = state.canvasTheme;
  switch (kind) {
    case EVENT_HALTED:
      return { label: "HALT", cls: "halt", color: t.haltStroke };
    case EVENT_RESUMED:
      return { label: "RESUME", cls: "reload", color: t.samplingColor };
    case EVENT_RESET:
      return { label: "RESET", cls: "reset", color: t.resetColor };
    case EVENT_RELOAD:
      return { label: "RELOAD", cls: "reload", color: t.reloadColor };
    case EVENT_FAULT:
      return { label: "FAULT", cls: "fault", color: t.faultColor };
    case EVENT_SAMPLING:
    default:
      return { label: "CFG", cls: "reload", color: t.samplingColor };
  }
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
  const font = `600 ${Math.round(10 * dpr)}px ui-monospace, monospace`;
  ctx.save();
  ctx.font = font;
  const padX = 4 * dpr;
  const padY = 2 * dpr;
  const w = measureMonoWidth(ctx, font, text) + padX * 2;
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

function drawHoverValueBox(
  ctx: CanvasRenderingContext2D,
  x: number,
  y: number,
  text: string,
  dotColor: string,
  state: TimelineState,
  plotRight: number,
  plotTop: number,
  plotBottom: number,
): void {
  const dpr = devicePixelRatio || 1;
  const theme = state.canvasTheme;
  const font = `600 ${Math.round(10 * dpr)}px ui-monospace, monospace`;
  ctx.save();
  ctx.font = font;
  const padX = 5 * dpr;
  const boxH = 15 * dpr;
  const textW = measureMonoWidth(ctx, font, text);
  const boxW = textW + padX * 2 + 7 * dpr;
  const placeLeft = x + 8 * dpr + boxW > plotRight;
  const bx = placeLeft ? x - 8 * dpr - boxW : x + 8 * dpr;
  const by = Math.max(plotTop + 2 * dpr, Math.min(plotBottom - boxH - 2 * dpr, y - boxH / 2));

  ctx.fillStyle = theme.hoverBoxBg;
  ctx.strokeStyle = dotColor;
  ctx.lineWidth = Math.max(1, dpr);
  ctx.beginPath();
  ctx.roundRect(bx, by, boxW, boxH, 3 * dpr);
  ctx.fill();
  ctx.stroke();

  ctx.fillStyle = dotColor;
  ctx.beginPath();
  ctx.arc(bx + 5 * dpr, by + boxH / 2, 2.5 * dpr, 0, Math.PI * 2);
  ctx.fill();

  ctx.fillStyle = theme.hoverBoxText;
  ctx.textBaseline = "middle";
  ctx.textAlign = "left";
  ctx.fillText(text, bx + 10 * dpr, by + boxH / 2);
  ctx.restore();
}

function drawValueBoxes(
  ctx: CanvasRenderingContext2D,
  x: number,
  visibleMembers: SeriesInfo[],
  t: number,
  state: TimelineState,
  toY: (value: number) => number,
  plotLeft: number,
  plotTop: number,
  plotWidth: number,
  plotHeight: number,
  dotColorOverride?: string,
): void {
  const dpr = devicePixelRatio || 1;
  const placedY: number[] = [];
  const minGap = 17 * dpr;
  for (const member of visibleMembers) {
    const v = valueAt(state, member, t);
    if (v === null) continue;
    const py = toY(v);
    if (py < plotTop || py > plotTop + plotHeight) continue;
    const color = colorOf(state, member);
    ctx.fillStyle = dotColorOverride ?? color;
    ctx.beginPath();
    ctx.arc(x, py, 3.5 * dpr, 0, Math.PI * 2);
    ctx.fill();

    let boxY = py;
    for (const prevY of placedY) {
      if (Math.abs(boxY - prevY) < minGap) {
        boxY = prevY + minGap;
      }
    }
    placedY.push(boxY);
    drawHoverValueBox(
      ctx,
      x,
      boxY,
      formatNumber(v, 5),
      color,
      state,
      plotLeft + plotWidth,
      plotTop,
      plotTop + plotHeight,
    );
  }
}

function syncOverlayCanvasSize(track: TrackRow): CanvasRenderingContext2D | null {
  const baseCanvas = track.plot.ctx.canvas;
  const overlay = track.overlayCanvas;
  const w = baseCanvas.width;
  const h = baseCanvas.height;
  if (w <= 0 || h <= 0) {
    return null;
  }
  if (overlay.width !== w || overlay.height !== h) {
    overlay.width = w;
    overlay.height = h;
  }
  const cssW = `${track.plot.width}px`;
  const cssH = `${track.plot.height}px`;
  if (overlay.style.width !== cssW) overlay.style.width = cssW;
  if (overlay.style.height !== cssH) overlay.style.height = cssH;
  return overlay.getContext("2d");
}

/**
 * Draws halts, event markers, box-zoom preview, measurement cursors, pins, and hover
 * value boxes onto the track's dedicated overlay canvas without touching uPlot's
 * waveform canvas.
 */
export function drawTrackOverlay(state: TimelineState, track: TrackRow): void {
  const context = syncOverlayCanvasSize(track);
  if (!context) return;

  const overlay = track.overlayCanvas;
  context.clearRect(0, 0, overlay.width, overlay.height);

  const plot = track.plot;
  const { left, top, width, height } = plot.bbox;
  if (width <= 0 || height <= 0) return;

  const min = plot.scales.x.min ?? 0;
  const max = plot.scales.x.max ?? 0;
  const dpr = devicePixelRatio || 1;
  const theme = state.canvasTheme;
  const toX = (value: number) => plot.valToPos(value, "x", true);
  const toY = (value: number) => plot.valToPos(value, "y", true);

  context.save();
  context.beginPath();
  context.rect(left, top, width, height);
  context.clip();

  // 1. Halted regions
  for (const halt of state.halts) {
    const end = halt.to ?? max;
    if (end >= min && halt.from <= max) {
      const x0 = toX(Math.max(halt.from, min));
      const x1 = toX(Math.min(end, max));
      const w = Math.max(1, x1 - x0);
      context.fillStyle = theme.haltFill;
      context.fillRect(x0, top, w, height);
      context.fillStyle = theme.haltStroke;
      context.fillRect(x0, top, w, Math.max(2, 2 * dpr));
      if (w > 54 * dpr) {
        const dur = (halt.to ?? nowSeconds(state)) - halt.from;
        drawBadge(
          context,
          x0 + w / 2,
          top + 4 * dpr,
          `HALT ${formatSeconds(dur)}`,
          theme.haltStroke,
          "#ffffff",
        );
      }
    }
  }

  // 2. Box-zoom selection preview
  if (state.boxSelect && state.boxSelect.groupName === track.name) {
    const bx0 = Math.min(state.boxSelect.x0, state.boxSelect.x1);
    const bw = Math.abs(state.boxSelect.x1 - state.boxSelect.x0);
    context.fillStyle = theme.selectionFill;
    context.fillRect(bx0, top, bw, height);
    context.strokeStyle = theme.selectionStroke;
    context.lineWidth = dpr;
    context.strokeRect(bx0 + 0.5, top + 0.5, bw, height - 1);
  }

  // 3. Cursor A..B shaded span
  if (state.cursorA !== null && state.cursorB !== null) {
    const cMin = Math.min(state.cursorA, state.cursorB);
    const cMax = Math.max(state.cursorA, state.cursorB);
    if (cMax >= min && cMin <= max) {
      const x0 = toX(Math.max(cMin, min));
      const x1 = toX(Math.min(cMax, max));
      context.fillStyle = theme.cursorSpanFill;
      context.fillRect(x0, top, x1 - x0, height);
    }
  }

  context.lineWidth = dpr;
  const vline = (value: number, style: string, dash: number[] = []) => {
    if (value >= min && value <= max) {
      const x = Math.round(toX(value)) + 0.5;
      context.save();
      context.strokeStyle = style;
      if (dash.length > 0) context.setLineDash(dash);
      context.beginPath();
      context.moveTo(x, top);
      context.lineTo(x, top + height);
      context.stroke();
      context.restore();
      return x;
    }
    return null;
  };

  // 4. Event Markers
  for (const marker of state.markers) {
    const info = eventKindInfo(state, marker.kind);
    const x = vline(
      marker.t,
      info.color,
      marker.kind !== EVENT_FAULT ? [4 * dpr, 3 * dpr] : [],
    );
    if (x !== null) {
      drawBadge(context, x, top + 3 * dpr, info.label, info.color, theme.badgeText, "left");
    }
  }

  const visibleMembers = track.members.filter((m) => isSeriesVisible(state, m));

  // 5. Sample Cursor (S)
  if (state.sampleCursor !== null) {
    const xs = vline(state.sampleCursor, theme.sampleCursor, [6 * dpr, 3 * dpr]);
    if (xs !== null) {
      drawBadge(context, xs, top + 2 * dpr, "S", theme.sampleCursor, "#111315", "center");
      drawValueBoxes(
        context,
        xs,
        visibleMembers,
        state.sampleCursor,
        state,
        toY,
        left,
        top,
        width,
        height,
        theme.sampleCursor,
      );
    }
  }

  // 5b. Pinned Markers (M1, M2, ...)
  for (const pin of state.pins) {
    const xp = vline(pin.t, theme.pinColor);
    if (xp !== null) {
      drawBadge(context, xp, top + 2 * dpr, pin.label, theme.pinColor, "#ffffff", "center");
      drawValueBoxes(
        context,
        xp,
        visibleMembers,
        pin.t,
        state,
        toY,
        left,
        top,
        width,
        height,
        theme.pinColor,
      );
    }
  }

  // 6. Hover Crosshair + Ozone intersection value boxes
  if (state.hoverTime !== null && state.hoverTime >= min && state.hoverTime <= max) {
    const hx = vline(state.hoverTime, theme.crosshair, [3 * dpr, 3 * dpr]);
    if (hx !== null) {
      drawValueBoxes(
        context,
        hx,
        visibleMembers,
        state.hoverTime,
        state,
        toY,
        left,
        top,
        width,
        height,
      );
      if (state.sampleCursor !== null) {
        const dt = state.hoverTime - state.sampleCursor;
        const align = hx + 90 * dpr > left + width ? "right" : "left";
        drawBadge(
          context,
          hx,
          top + 18 * dpr,
          `Δt ${formatSeconds(dt)}`,
          theme.sampleCursor,
          "#111315",
          align,
        );
      }
    }
  }

  // 7. Measurement Cursors A & B
  if (state.cursorA !== null) {
    const xa = vline(state.cursorA, theme.cursorA);
    if (xa !== null) {
      drawBadge(context, xa, top + 2 * dpr, "A", theme.cursorA, "#ffffff", "right");
      for (const member of visibleMembers) {
        const v = valueAt(state, member, state.cursorA);
        if (v !== null) {
          const py = toY(v);
          if (py >= top && py <= top + height) {
            context.fillStyle = theme.cursorA;
            context.beginPath();
            context.arc(xa, py, 3.5 * dpr, 0, Math.PI * 2);
            context.fill();
          }
        }
      }
    }
  }
  if (state.cursorB !== null) {
    const xb = vline(state.cursorB, theme.cursorB);
    if (xb !== null) {
      drawBadge(context, xb, top + 2 * dpr, "B", theme.cursorB, "#ffffff", "left");
      for (const member of visibleMembers) {
        const v = valueAt(state, member, state.cursorB);
        if (v !== null) {
          const py = toY(v);
          if (py >= top && py <= top + height) {
            context.fillStyle = theme.cursorB;
            context.beginPath();
            context.arc(xb, py, 3.5 * dpr, 0, Math.PI * 2);
            context.fill();
          }
        }
      }
    }
  }

  if (
    state.cursorA !== null &&
    state.cursorB !== null &&
    state.cursorA >= min &&
    state.cursorB >= min &&
    state.cursorA <= max &&
    state.cursorB <= max
  ) {
    const midX = (toX(state.cursorA) + toX(state.cursorB)) / 2;
    const dt = state.cursorB - state.cursorA;
    if (Math.abs(toX(state.cursorB) - toX(state.cursorA)) > 75 * dpr) {
      drawBadge(
        context,
        midX,
        top + 2 * dpr,
        `Δt ${formatSeconds(dt)} (${formatFrequency(dt)})`,
        theme.cursorA,
        "#ffffff",
      );
    }
  }

  context.restore();
}

export interface SharedAxisMetrics {
  leftPx: number;
  plotWidthPx: number;
  tStart: number;
  tEnd: number;
}

export function getSharedAxisMetrics(
  canvasWidthCss: number,
  state: TimelineState,
): SharedAxisMetrics {
  const dpr = devicePixelRatio || 1;
  const firstPlot = state.tracks[0]?.plot;
  const leftPx = firstPlot ? firstPlot.bbox.left / dpr : 64;
  const plotWidthPx = firstPlot
    ? Math.max(10, firstPlot.bbox.width / dpr)
    : Math.max(10, canvasWidthCss - leftPx);
  const tEnd = state.lastColumns
    ? state.lastColumns.result.t1 / 1e9
    : state.followLive
      ? nowSeconds(state) - 0.03
      : state.viewEnd;
  const tStart = state.lastColumns
    ? state.lastColumns.result.t0 / 1e9
    : tEnd - spanSeconds(state.division);
  return { leftPx, plotWidthPx, tStart, tEnd };
}

export function drawSharedAxis(canvas: HTMLCanvasElement, state: TimelineState): void {
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

  const theme = state.canvasTheme;
  ctx.clearRect(0, 0, w, h);

  const metrics = getSharedAxisMetrics(rect.width, state);
  const left = metrics.leftPx * dpr;
  const plotW = metrics.plotWidthPx * dpr;
  const { tStart, tEnd } = metrics;

  ctx.strokeStyle = theme.tickStroke;
  ctx.lineWidth = Math.max(1, dpr);
  ctx.beginPath();
  ctx.moveTo(left, 0.5);
  ctx.lineTo(left + plotW, 0.5);
  ctx.stroke();

  const splits = divisionSplits(state.division, tStart, tEnd);
  const labels = divisionLabels(state.division, splits);
  ctx.font = `${Math.round(11 * dpr)}px ui-monospace, monospace`;
  ctx.textAlign = "center";
  ctx.textBaseline = "top";

  for (let i = 0; i < splits.length; i++) {
    const x = left + timeToPx(splits[i], plotW, tStart, tEnd);
    if (x < left - 2 || x > left + plotW + 2) continue;
    ctx.strokeStyle = theme.tickStroke;
    ctx.beginPath();
    ctx.moveTo(Math.round(x) + 0.5, 0);
    ctx.lineTo(Math.round(x) + 0.5, 5 * dpr);
    ctx.stroke();

    ctx.fillStyle = theme.axisStroke;
    ctx.fillText(labels[i], x, 7 * dpr);
  }

  const drawHandle = (tVal: number | null, color: string, label: string, fillText: string) => {
    if (tVal === null || tVal < tStart || tVal > tEnd) return;
    const x = left + timeToPx(tVal, plotW, tStart, tEnd);
    const hw = Math.max(7, 4 + label.length * 2.5) * dpr;
    const hh = 11 * dpr;
    ctx.save();
    ctx.fillStyle = color;
    ctx.beginPath();
    ctx.moveTo(x, 0);
    ctx.lineTo(x - hw, hh);
    ctx.lineTo(x + hw, hh);
    ctx.closePath();
    ctx.fill();
    ctx.font = `700 ${Math.round(8.5 * dpr)}px ui-monospace, monospace`;
    ctx.fillStyle = fillText;
    ctx.textAlign = "center";
    ctx.textBaseline = "bottom";
    ctx.fillText(label, x, hh + 0.5 * dpr);
    ctx.restore();
  };

  drawHandle(state.sampleCursor, theme.sampleCursor, "S", "#111315");
  for (const pin of state.pins) {
    drawHandle(pin.t, theme.pinColor, pin.label, "#ffffff");
  }
  drawHandle(state.cursorA, theme.cursorA, "A", "#ffffff");
  drawHandle(state.cursorB, theme.cursorB, "B", "#ffffff");
}

type AxisDragTarget = { kind: "S" | "A" | "B" } | { kind: "pin"; id: number };

export function attachSharedAxisInteractions(
  canvas: HTMLCanvasElement,
  state: TimelineState,
  onCursorMoved: () => void,
): void {
  let dragging: AxisDragTarget | null = null;
  let pendingClickX: number | null = null;

  const hitCursor = (clientX: number): AxisDragTarget | null => {
    const rect = canvas.getBoundingClientRect();
    const metrics = getSharedAxisMetrics(rect.width, state);
    const localX = clientX - rect.left - metrics.leftPx;
    const check = (val: number | null): boolean =>
      val !== null && Math.abs(localX - timeToPx(val, metrics.plotWidthPx, metrics.tStart, metrics.tEnd)) <= 9;

    if (check(state.cursorB)) return { kind: "B" };
    if (check(state.cursorA)) return { kind: "A" };
    for (let i = state.pins.length - 1; i >= 0; i--) {
      if (check(state.pins[i].t)) return { kind: "pin", id: state.pins[i].id };
    }
    if (check(state.sampleCursor)) return { kind: "S" };
    return null;
  };

  const timeFromClientX = (clientX: number): number => {
    const rect = canvas.getBoundingClientRect();
    const metrics = getSharedAxisMetrics(rect.width, state);
    const localX = clientX - rect.left - metrics.leftPx;
    return pxToTime(localX, metrics.plotWidthPx, metrics.tStart, metrics.tEnd);
  };

  canvas.addEventListener("mousedown", (event) => {
    if (event.button !== 0) return;
    const hit = hitCursor(event.clientX);
    if (hit) {
      dragging = hit;
      pendingClickX = null;
      event.preventDefault();
    } else {
      pendingClickX = event.clientX;
    }
  });

  canvas.addEventListener("dblclick", (event) => {
    state.sampleCursor = timeFromClientX(event.clientX);
    state.sampleCursorPinned = true;
    onCursorMoved();
  });

  canvas.addEventListener("mousemove", (event) => {
    if (!dragging) {
      const nextCursor = hitCursor(event.clientX) ? "ew-resize" : "default";
      if (canvas.style.cursor !== nextCursor) {
        canvas.style.cursor = nextCursor;
      }
    }
  });

  window.addEventListener("mousemove", (event) => {
    if (!dragging) return;
    const t = timeFromClientX(event.clientX);
    if (dragging.kind === "S") state.sampleCursor = t;
    else if (dragging.kind === "A") state.cursorA = t;
    else if (dragging.kind === "B") state.cursorB = t;
    else if (dragging.kind === "pin") {
      const pinId = dragging.id;
      const pin = state.pins.find((p) => p.id === pinId);
      if (pin) pin.t = t;
    }
    onCursorMoved();
  });

  window.addEventListener("mouseup", (event) => {
    if (!dragging && pendingClickX !== null && Math.abs(event.clientX - pendingClickX) < 4) {
      state.sampleCursor = timeFromClientX(event.clientX);
      state.sampleCursorPinned = true;
      onCursorMoved();
    }
    pendingClickX = null;
    dragging = null;
  });
}

interface InspectorDomCache {
  drawer: HTMLElement | null;
  pane: HTMLElement | null;
  output: HTMLElement | null;
  sEl: HTMLElement | null;
  aEl: HTMLElement | null;
  bEl: HTMLElement | null;
  dtEl: HTMLElement | null;
  freqEl: HTMLElement | null;
  pinsWrap: HTMLElement | null;
  zoomBtn: HTMLButtonElement | null;
  clearBtn: HTMLButtonElement | null;
  thead: HTMLTableSectionElement | null;
  tbody: HTMLTableSectionElement | null;
  pinValEls: Map<number, HTMLElement>;
  pinKey: string;
  headerPinKey: string;
  rowStructureKey: string;
  rowCells: HTMLTableCellElement[][];
}

let inspectorCache: InspectorDomCache | null = null;

function getInspectorDom(): InspectorDomCache {
  if (!inspectorCache) {
    const byId = <T extends HTMLElement>(id: string) => document.getElementById(id) as T | null;
    inspectorCache = {
      drawer: byId("bottom-drawer"),
      pane: byId("drawer-cursors"),
      output: byId("measure"),
      sEl: byId("cursor-s-val"),
      aEl: byId("cursor-a-val"),
      bEl: byId("cursor-b-val"),
      dtEl: byId("cursor-dt-val"),
      freqEl: byId("cursor-freq-val"),
      pinsWrap: byId("cursor-pins"),
      zoomBtn: byId<HTMLButtonElement>("cursor-zoom"),
      clearBtn: byId<HTMLButtonElement>("cursor-clear"),
      thead: byId<HTMLTableSectionElement>("cursor-thead"),
      tbody: byId<HTMLTableSectionElement>("cursor-tbody"),
      pinValEls: new Map(),
      pinKey: "",
      headerPinKey: "",
      rowStructureKey: "",
      rowCells: [],
    };
  }
  return inspectorCache;
}

function setTextIfChanged(el: HTMLElement | null, text: string): void {
  if (el && el.textContent !== text) {
    el.textContent = text;
  }
}

export function updateCursorInspector(state: TimelineState): void {
  const dom = getInspectorDom();
  const visibleSeries = state.series.filter((item) => isSeriesVisible(state, item));

  if (dom.output) {
    let text: string;
    if (state.cursorA !== null && state.cursorB !== null) {
      const dt = state.cursorB - state.cursorA;
      text = `Δt = ${formatSeconds(dt)} (${formatFrequency(dt)})`;
      const first = visibleSeries[0];
      if (first) {
        const a = valueAt(state, first, state.cursorA);
        const b = valueAt(state, first, state.cursorB);
        if (a !== null && b !== null) {
          text += `  Δ${first.name} = ${(b - a).toPrecision(5)}`;
        }
      }
    } else if (state.cursorA !== null) {
      text = `A = ${state.cursorA.toFixed(4)} s (press B to measure Δt)`;
    } else if (state.sampleCursor !== null) {
      text = `Sample S = ${state.sampleCursor.toFixed(4)} s`;
    } else {
      text = "Click to place Sample S; A / B keys place measurement cursors.";
    }
    setTextIfChanged(dom.output, text);
  }

  // Skip all drawer table/chip DOM work when the cursor inspector pane is closed.
  if (!dom.drawer || dom.drawer.hidden || !dom.pane || dom.pane.hidden) {
    return;
  }

  setTextIfChanged(dom.sEl, state.sampleCursor !== null ? `${state.sampleCursor.toFixed(4)} s` : "—");
  setTextIfChanged(dom.aEl, state.cursorA !== null ? `${state.cursorA.toFixed(4)} s` : "—");
  setTextIfChanged(dom.bEl, state.cursorB !== null ? `${state.cursorB.toFixed(4)} s` : "—");

  const pinIdsKey = state.pins.map((p) => p.id).join(",");
  if (dom.pinsWrap) {
    if (dom.pinKey !== pinIdsKey) {
      dom.pinKey = pinIdsKey;
      dom.pinValEls.clear();
      dom.pinsWrap.replaceChildren(
        ...state.pins.map((pin) => {
          const chip = document.createElement("div");
          chip.className = "cursor-chip cursor-pin";
          const lbl = document.createElement("span");
          lbl.className = "chip-label";
          lbl.textContent = pin.label;
          const val = document.createElement("span");
          val.className = "chip-val";
          val.textContent = `${pin.t.toFixed(4)} s`;
          dom.pinValEls.set(pin.id, val);
          chip.append(lbl, val);
          return chip;
        }),
      );
    } else {
      for (const pin of state.pins) {
        setTextIfChanged(dom.pinValEls.get(pin.id) ?? null, `${pin.t.toFixed(4)} s`);
      }
    }
  }

  const hideClear = state.cursorA === null && state.cursorB === null && state.sampleCursor === null;
  if (dom.clearBtn && dom.clearBtn.hidden !== hideClear) dom.clearBtn.hidden = hideClear;
  const hideZoom = state.cursorA === null || state.cursorB === null;
  if (dom.zoomBtn && dom.zoomBtn.hidden !== hideZoom) dom.zoomBtn.hidden = hideZoom;

  const headerPinSpec = state.pins.map((p) => `${p.id}:${p.t.toFixed(3)}`).join(",");
  if (dom.thead && dom.headerPinKey !== headerPinSpec) {
    dom.headerPinKey = headerPinSpec;
    const headerRow = document.createElement("tr");
    for (const label of ["Signal", "S", "A", "B", "ΔV (B−A)", "dV/dt"]) {
      const th = document.createElement("th");
      th.textContent = label;
      headerRow.append(th);
    }
    for (const pin of state.pins) {
      const th = document.createElement("th");
      th.textContent = `${pin.label} (${pin.t.toFixed(3)}s)`;
      th.title = `${pin.label} @ ${pin.t.toFixed(4)} s`;
      headerRow.append(th);
    }
    dom.thead.replaceChildren(headerRow);
  }

  if (!dom.tbody) return;
  if (
    state.cursorA === null &&
    state.cursorB === null &&
    state.sampleCursor === null &&
    state.pins.length === 0
  ) {
    if (dom.rowStructureKey !== "") {
      dom.rowStructureKey = "";
      dom.rowCells = [];
      dom.tbody.replaceChildren();
    }
    setTextIfChanged(dom.dtEl, "—");
    setTextIfChanged(dom.freqEl, "—");
    return;
  }

  const hasAB = state.cursorA !== null && state.cursorB !== null;
  const dt = hasAB ? state.cursorB! - state.cursorA! : 0;
  setTextIfChanged(dom.dtEl, hasAB ? formatSeconds(dt) : "—");
  setTextIfChanged(dom.freqEl, hasAB ? formatFrequency(dt) : "—");

  const nextRowKey = `${visibleSeries.map((s) => s.seriesId).join(",")}|${pinIdsKey}`;
  const colCount = 6 + state.pins.length;
  if (dom.rowStructureKey !== nextRowKey) {
    dom.rowStructureKey = nextRowKey;
    dom.rowCells = [];
    const rows = visibleSeries.map((item) => {
      const tr = document.createElement("tr");
      const cells: HTMLTableCellElement[] = [];
      for (let c = 0; c < colCount; c++) {
        const td = document.createElement("td");
        if (c === 0) {
          td.title = item.name;
          td.textContent = item.name;
        }
        cells.push(td);
        tr.append(td);
      }
      dom.rowCells.push(cells);
      return tr;
    });
    dom.tbody.replaceChildren(...rows);
  }

  for (let r = 0; r < visibleSeries.length; r++) {
    const item = visibleSeries[r];
    const cells = dom.rowCells[r];
    if (!cells) continue;
    const vs = state.sampleCursor !== null ? valueAt(state, item, state.sampleCursor) : null;
    const va = state.cursorA !== null ? valueAt(state, item, state.cursorA) : null;
    const vb = state.cursorB !== null ? valueAt(state, item, state.cursorB) : null;
    const dv = va !== null && vb !== null ? vb - va : null;
    const slope = dv !== null && Math.abs(dt) > 1e-12 ? dv / dt : null;

    setTextIfChanged(cells[0], item.name);
    setTextIfChanged(cells[1], formatNumber(vs, 5));
    setTextIfChanged(cells[2], formatNumber(va, 5));
    setTextIfChanged(cells[3], formatNumber(vb, 5));
    setTextIfChanged(cells[4], formatNumber(dv, 5));
    setTextIfChanged(cells[5], slope !== null ? `${formatNumber(slope, 4)}/s` : "—");
    for (let p = 0; p < state.pins.length; p++) {
      setTextIfChanged(cells[6 + p], formatNumber(valueAt(state, item, state.pins[p].t), 5));
    }
  }
}
