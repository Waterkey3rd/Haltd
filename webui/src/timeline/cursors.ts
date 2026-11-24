import type uPlot from "uplot";
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
} from "./state";

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
  ctx.save();
  ctx.font = `600 ${Math.round(10 * dpr)}px ui-monospace, monospace`;
  const padX = 5 * dpr;
  const boxH = 15 * dpr;
  const textW = ctx.measureText(text).width;
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

export function drawOverlays(
  state: TimelineState,
  groupName: string,
  members: SeriesInfo[],
): (plot: uPlot) => void {
  return (plot: uPlot): void => {
    const context = plot.ctx;
    const { left, top, width, height } = plot.bbox;
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
    if (state.boxSelect && state.boxSelect.groupName === groupName) {
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

    const visibleMembers = members.filter((m) => isSeriesVisible(state, m));

    // 5. Sample Cursor (S)
    if (state.sampleCursor !== null) {
      const xs = vline(state.sampleCursor, theme.sampleCursor, [6 * dpr, 3 * dpr]);
      if (xs !== null) {
        drawBadge(context, xs, top + 2 * dpr, "S", theme.sampleCursor, "#111315", "center");
        for (const member of visibleMembers) {
          const v = valueAt(state, member, state.sampleCursor);
          if (v !== null) {
            const py = toY(v);
            if (py >= top && py <= top + height) {
              context.fillStyle = theme.sampleCursor;
              context.beginPath();
              context.arc(xs, py, 3.5 * dpr, 0, Math.PI * 2);
              context.fill();
            }
          }
        }
      }
    }

    // 6. Hover Crosshair + Ozone intersection value boxes
    if (state.hoverTime !== null && state.hoverTime >= min && state.hoverTime <= max) {
      const hx = vline(state.hoverTime, theme.crosshair, [3 * dpr, 3 * dpr]);
      if (hx !== null) {
        const placedY: number[] = [];
        const minGap = 17 * dpr;
        for (const member of visibleMembers) {
          const v = valueAt(state, member, state.hoverTime);
          if (v === null) continue;
          const py = toY(v);
          if (py < top || py > top + height) continue;
          const color = colorOf(state, member, state.series.indexOf(member));
          context.fillStyle = color;
          context.beginPath();
          context.arc(hx, py, 3.5 * dpr, 0, Math.PI * 2);
          context.fill();

          let boxY = py;
          for (const prevY of placedY) {
            if (Math.abs(boxY - prevY) < minGap) {
              boxY = prevY + minGap;
            }
          }
          placedY.push(boxY);
          drawHoverValueBox(
            context,
            hx,
            boxY,
            formatNumber(v, 5),
            color,
            state,
            left + width,
            top,
            top + height,
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
  };
}

export interface SharedAxisMetrics {
  leftPx: number;
  plotWidthPx: number;
  tStart: number;
  tEnd: number;
}

export function getSharedAxisMetrics(
  canvas: HTMLCanvasElement,
  state: TimelineState,
): SharedAxisMetrics {
  const rect = canvas.getBoundingClientRect();
  const dpr = devicePixelRatio || 1;
  const firstPlot = state.tracks[0]?.plot;
  const leftPx = firstPlot ? firstPlot.bbox.left / dpr : 64;
  const plotWidthPx = firstPlot
    ? Math.max(10, firstPlot.bbox.width / dpr)
    : Math.max(10, rect.width - leftPx);
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

  const metrics = getSharedAxisMetrics(canvas, state);
  const left = metrics.leftPx * dpr;
  const plotW = metrics.plotWidthPx * dpr;
  const { tStart, tEnd } = metrics;

  // Baseline
  ctx.strokeStyle = theme.tickStroke;
  ctx.lineWidth = Math.max(1, dpr);
  ctx.beginPath();
  ctx.moveTo(left, 0.5);
  ctx.lineTo(left + plotW, 0.5);
  ctx.stroke();

  // Division ticks & labels
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

  // Draggable triangular cursor handles (S, A, B)
  const drawHandle = (tVal: number | null, color: string, label: string, fillText: string) => {
    if (tVal === null || tVal < tStart || tVal > tEnd) return;
    const x = left + timeToPx(tVal, plotW, tStart, tEnd);
    const hw = 7 * dpr;
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
  drawHandle(state.cursorA, theme.cursorA, "A", "#ffffff");
  drawHandle(state.cursorB, theme.cursorB, "B", "#ffffff");
}

export function attachSharedAxisInteractions(
  canvas: HTMLCanvasElement,
  state: TimelineState,
  onCursorMoved: () => void,
): void {
  let dragging: "S" | "A" | "B" | null = null;

  const hitCursor = (clientX: number): "S" | "A" | "B" | null => {
    const rect = canvas.getBoundingClientRect();
    const metrics = getSharedAxisMetrics(canvas, state);
    const localX = clientX - rect.left - metrics.leftPx;
    const candidates: Array<{ kind: "S" | "A" | "B"; val: number | null }> = [
      { kind: "B", val: state.cursorB },
      { kind: "A", val: state.cursorA },
      { kind: "S", val: state.sampleCursor },
    ];
    for (const c of candidates) {
      if (c.val === null) continue;
      const cx = timeToPx(c.val, metrics.plotWidthPx, metrics.tStart, metrics.tEnd);
      if (Math.abs(localX - cx) <= 9) {
        return c.kind;
      }
    }
    return null;
  };

  const timeFromClientX = (clientX: number): number => {
    const rect = canvas.getBoundingClientRect();
    const metrics = getSharedAxisMetrics(canvas, state);
    const localX = clientX - rect.left - metrics.leftPx;
    return pxToTime(localX, metrics.plotWidthPx, metrics.tStart, metrics.tEnd);
  };

  canvas.addEventListener("mousedown", (event) => {
    if (event.button !== 0) return;
    const hit = hitCursor(event.clientX);
    if (hit) {
      dragging = hit;
      event.preventDefault();
    }
  });

  canvas.addEventListener("dblclick", (event) => {
    state.sampleCursor = timeFromClientX(event.clientX);
    state.sampleCursorPinned = true;
    onCursorMoved();
  });

  canvas.addEventListener("mousemove", (event) => {
    if (!dragging) {
      canvas.style.cursor = hitCursor(event.clientX) ? "ew-resize" : "default";
    }
  });

  window.addEventListener("mousemove", (event) => {
    if (!dragging) return;
    const t = timeFromClientX(event.clientX);
    if (dragging === "S") state.sampleCursor = t;
    if (dragging === "A") state.cursorA = t;
    if (dragging === "B") state.cursorB = t;
    onCursorMoved();
  });

  window.addEventListener("mouseup", () => {
    dragging = null;
  });
}

export function updateCursorInspector(state: TimelineState): void {
  const byId = <T extends HTMLElement>(id: string) => document.getElementById(id) as T | null;
  const output = byId("measure");
  const sEl = byId("cursor-s-val");
  const aEl = byId("cursor-a-val");
  const bEl = byId("cursor-b-val");
  const dtEl = byId("cursor-dt-val");
  const freqEl = byId("cursor-freq-val");
  const zoomBtn = byId<HTMLButtonElement>("cursor-zoom");
  const clearBtn = byId<HTMLButtonElement>("cursor-clear");
  const tbody = byId<HTMLTableSectionElement>("cursor-tbody");

  if (sEl) sEl.textContent = state.sampleCursor !== null ? `${state.sampleCursor.toFixed(4)} s` : "—";
  if (aEl) aEl.textContent = state.cursorA !== null ? `${state.cursorA.toFixed(4)} s` : "—";
  if (bEl) bEl.textContent = state.cursorB !== null ? `${state.cursorB.toFixed(4)} s` : "—";
  if (clearBtn) {
    clearBtn.hidden =
      state.cursorA === null && state.cursorB === null && state.sampleCursor === null;
  }
  if (zoomBtn) {
    zoomBtn.hidden = state.cursorA === null || state.cursorB === null;
  }

  const visibleSeries = state.series.filter((item) => isSeriesVisible(state, item));

  if (output) {
    if (state.cursorA !== null && state.cursorB !== null) {
      const dt = state.cursorB - state.cursorA;
      let text = `Δt = ${formatSeconds(dt)} (${formatFrequency(dt)})`;
      const first = visibleSeries[0];
      if (first) {
        const a = valueAt(state, first, state.cursorA);
        const b = valueAt(state, first, state.cursorB);
        if (a !== null && b !== null) {
          text += `  Δ${first.name} = ${(b - a).toPrecision(5)}`;
        }
      }
      output.textContent = text;
    } else if (state.cursorA !== null) {
      output.textContent = `A = ${state.cursorA.toFixed(4)} s (press B to measure Δt)`;
    } else if (state.sampleCursor !== null) {
      output.textContent = `Sample S = ${state.sampleCursor.toFixed(4)} s`;
    } else {
      output.textContent = "Double-click to place Sample S; A / B keys place measurement cursors.";
    }
  }

  if (!tbody) return;
  if (state.cursorA === null && state.cursorB === null && state.sampleCursor === null) {
    tbody.replaceChildren();
    return;
  }

  const hasAB = state.cursorA !== null && state.cursorB !== null;
  const dt = hasAB ? state.cursorB! - state.cursorA! : 0;

  tbody.replaceChildren(
    ...visibleSeries.map((item) => {
      const tr = document.createElement("tr");
      const vs = state.sampleCursor !== null ? valueAt(state, item, state.sampleCursor) : null;
      const va = state.cursorA !== null ? valueAt(state, item, state.cursorA) : null;
      const vb = state.cursorB !== null ? valueAt(state, item, state.cursorB) : null;
      const dv = va !== null && vb !== null ? vb - va : null;
      const slope = dv !== null && Math.abs(dt) > 1e-12 ? dv / dt : null;

      const tdName = document.createElement("td");
      tdName.title = item.name;
      tdName.textContent = item.name;
      const tdS = document.createElement("td");
      tdS.textContent = formatNumber(vs, 5);
      const tdA = document.createElement("td");
      tdA.textContent = formatNumber(va, 5);
      const tdB = document.createElement("td");
      tdB.textContent = formatNumber(vb, 5);
      const tdDv = document.createElement("td");
      tdDv.textContent = formatNumber(dv, 5);
      const tdSlope = document.createElement("td");
      tdSlope.textContent = slope !== null ? `${formatNumber(slope, 4)}/s` : "—";
      tr.append(tdName, tdS, tdA, tdB, tdDv, tdSlope);
      return tr;
    }),
  );

  if (dtEl) dtEl.textContent = hasAB ? formatSeconds(dt) : "—";
  if (freqEl) freqEl.textContent = hasAB ? formatFrequency(dt) : "—";
}
