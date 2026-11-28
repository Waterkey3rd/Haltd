import uPlot from "uplot";

export interface HaltSpan {
  from: number;
  to: number | null;
}

export function hasHaltBetween(
  halts: readonly HaltSpan[],
  tPrev: number,
  tCurr: number,
): boolean {
  if (halts.length === 0 || tCurr - tPrev <= 0.003) {
    return false;
  }
  for (let i = halts.length - 1; i >= 0; i--) {
    const h = halts[i];
    const hEnd = h.to ?? Infinity;
    if (hEnd < tPrev) {
      break;
    }
    if (h.from < tCurr && hEnd > tPrev) {
      return true;
    }
  }
  return false;
}

/**
 * Returning `null` tells uPlot to skip `Path2D` construction, `boundsClip`,
 * and `ctx.fill/stroke(Path2D)`, allowing `drawSeries` to render directly.
 */
export const nullPathBuilder: uPlot.Series.PathBuilder = () => null;

/**
 * Direct Canvas2D DSO series renderer called from uPlot's `drawSeries` hook.
 * - Sparse signals render crisp rectangular-clipped `ctx.stroke()` polylines.
 * - Dense oscillating signals bypass `Path2D` tessellation and `ctx.clip(Path2D)`
 *   completely, drawing clamped integer `ctx.fillRect()` column bars at native blit speed.
 */
export function drawDsoSeries(
  u: uPlot,
  seriesIdx: number,
  halts: readonly HaltSpan[],
): void {
  if (seriesIdx <= 0) {
    return;
  }
  const s = u.series[seriesIdx];
  if (!s || !s.show) {
    return;
  }
  const xDim = u.bbox.width;
  const yDim = u.bbox.height;
  if (xDim <= 0 || yDim <= 0) {
    return;
  }

  const dataX = u.data[0];
  const dataY = u.data[seriesIdx];
  if (!dataX || !dataY || dataX.length === 0) {
    return;
  }

  const rawI0 = s.idxs?.[0] ?? 0;
  const rawI1 = s.idxs?.[1] ?? dataX.length - 1;
  if (rawI1 < rawI0) {
    return;
  }
  const i0 = Math.max(0, rawI0 - 1);
  const i1 = Math.min(dataX.length - 1, rawI1 + 1);

  const xMin = u.scales.x.min;
  const xMax = u.scales.x.max;
  const yMin = u.scales.y.min;
  const yMax = u.scales.y.max;
  if (
    xMin == null ||
    xMax == null ||
    yMin == null ||
    yMax == null ||
    !Number.isFinite(xMin) ||
    !Number.isFinite(xMax) ||
    !Number.isFinite(yMin) ||
    !Number.isFinite(yMax) ||
    xMax <= xMin ||
    yMax <= yMin
  ) {
    return;
  }

  const rawStroke = s.stroke;
  const strokeColor =
    typeof rawStroke === "function" ? rawStroke(u, seriesIdx) : rawStroke;
  if (!strokeColor) {
    return;
  }

  const dpr =
    uPlot.pxRatio || (typeof devicePixelRatio === "number" ? devicePixelRatio : 1) || 1;
  const xOff = u.bbox.left;
  const yOff = u.bbox.top;
  const xRight = xOff + xDim;
  const yBottom = yOff + yDim;
  const xScale = xDim / (xMax - xMin);
  const yScale = yDim / (yMax - yMin);
  const lineWidthPx = Math.max(1, (s.width ?? 1.05) * dpr);

  const ctx = u.ctx;
  const xDimCss = xDim / dpr;
  const sliceLen = i1 - i0 + 1;
  let useBars = sliceLen > xDimCss * 0.85;

  if (!useBars && sliceLen > 256) {
    let validCount = 0;
    let estVertTravel = 0;
    let prevPy = NaN;
    for (let i = i0; i <= i1; i++) {
      const v = dataY[i];
      if (v == null || !(v === v)) {
        continue;
      }
      validCount++;
      const py = yBottom - (v - yMin) * yScale;
      if (prevPy === prevPy) {
        estVertTravel += Math.abs(py - prevPy);
      }
      prevPy = py;
    }
    if (validCount > xDimCss * 0.85 || (validCount > 256 && estVertTravel > xDim * 4)) {
      useBars = true;
    }
  }

  if (!useBars) {
    let hasSegment = false;
    let prevT = 0;
    let drewAny = false;

    ctx.save();
    ctx.beginPath();
    ctx.rect(xOff, yOff, xDim, yDim);
    ctx.clip();

    ctx.strokeStyle = strokeColor;
    ctx.lineWidth = lineWidthPx;
    ctx.lineJoin = "round";
    ctx.lineCap = "round";
    ctx.beginPath();

    for (let i = i0; i <= i1; i++) {
      const v = dataY[i];
      if (v === null) {
        hasSegment = false;
        continue;
      }
      if (v === undefined || !(v === v)) {
        continue;
      }
      const t = dataX[i];
      const px = xOff + (t - xMin) * xScale;
      const py = yBottom - (v - yMin) * yScale;
      if (!hasSegment || hasHaltBetween(halts, prevT, t)) {
        ctx.moveTo(px, py);
        hasSegment = true;
      } else {
        ctx.lineTo(px, py);
      }
      prevT = t;
      drewAny = true;
    }

    if (drewAny) {
      ctx.stroke();
    }
    ctx.restore();
    return;
  }

  // Dense DSO Column Mode: direct clamped fillRect per pixel column (no Path2D, no clip).
  const barW = Math.max(1, Math.round(lineWidthPx));
  const sparseSegs: number[] = [];
  ctx.fillStyle = strokeColor;

  let hasCol = false;
  let curPx = 0;
  let minPy = Infinity;
  let maxPy = -Infinity;
  let firstPy = 0;
  let lastPy = 0;
  let colFirstT = 0;
  let colLastT = 0;

  let hasPrevCol = false;
  let prevPx = 0;
  let prevLastPy = 0;
  let prevLastT = 0;

  const flushColumn = () => {
    if (!hasCol) {
      return;
    }
    const haltedFromPrev = hasPrevCol && hasHaltBetween(halts, prevLastT, colFirstT);
    let topY = minPy;
    let botY = maxPy;

    if (hasPrevCol && !haltedFromPrev) {
      if (curPx - prevPx <= barW) {
        if (prevLastPy < topY) topY = prevLastPy;
        if (prevLastPy > botY) botY = prevLastPy;
      } else {
        sparseSegs.push(prevPx + barW * 0.5, prevLastPy, curPx + barW * 0.5, firstPy);
      }
    }

    if (curPx + barW > xOff && curPx < xRight) {
      const rx = Math.max(xOff, curPx);
      const rw = Math.min(xRight, curPx + barW) - rx;
      const yTop = Math.round(topY);
      const yBot = Math.round(Math.max(topY + barW, botY));
      if (rw > 0 && yBot > yOff && yTop < yBottom) {
        const ry = Math.max(yOff, yTop);
        const rh = Math.max(1, Math.min(yBottom, yBot) - ry);
        ctx.fillRect(rx, ry, rw, rh);
      }
    }

    hasPrevCol = true;
    prevPx = curPx;
    prevLastPy = lastPy;
    prevLastT = colLastT;
  };

  for (let i = i0; i <= i1; i++) {
    const v = dataY[i];
    if (v === null) {
      flushColumn();
      hasCol = false;
      hasPrevCol = false;
      continue;
    }
    if (v === undefined || !(v === v)) {
      continue;
    }
    const t = dataX[i];
    const px = Math.round(xOff + (t - xMin) * xScale);
    const py = yBottom - (v - yMin) * yScale;

    if (!hasCol) {
      hasCol = true;
      curPx = px;
      minPy = py;
      maxPy = py;
      firstPy = py;
      lastPy = py;
      colFirstT = t;
      colLastT = t;
    } else if (px === curPx && !hasHaltBetween(halts, colLastT, t)) {
      if (py < minPy) minPy = py;
      if (py > maxPy) maxPy = py;
      lastPy = py;
      colLastT = t;
    } else {
      flushColumn();
      hasCol = true;
      curPx = px;
      minPy = py;
      maxPy = py;
      firstPy = py;
      lastPy = py;
      colFirstT = t;
      colLastT = t;
    }
  }
  flushColumn();

  if (sparseSegs.length > 0) {
    ctx.save();
    ctx.beginPath();
    ctx.rect(xOff, yOff, xDim, yDim);
    ctx.clip();
    ctx.strokeStyle = strokeColor;
    ctx.lineWidth = lineWidthPx;
    ctx.lineJoin = "round";
    ctx.lineCap = "round";
    ctx.beginPath();
    for (let i = 0; i < sparseSegs.length; i += 4) {
      ctx.moveTo(sparseSegs[i], sparseSegs[i + 1]);
      ctx.lineTo(sparseSegs[i + 2], sparseSegs[i + 3]);
    }
    ctx.stroke();
    ctx.restore();
  }
}
