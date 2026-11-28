import { describe, expect, it } from "vitest";
import uPlot from "uplot";
import { drawDsoSeries, hasHaltBetween, nullPathBuilder, type HaltSpan } from "./dsoPaths";

interface RecordedDrawCalls {
  fillRects: Array<{ x: number; y: number; w: number; h: number }>;
  moveTos: number;
  lineTos: number;
  strokes: number;
  clips: number;
  clipsUsed: number;
}

function makeFakePlot(
  xs: ArrayLike<number>,
  ys: ArrayLike<number>,
  widthPx: number,
  heightPx: number,
  xOff = 10,
  yOff = 20,
): { plot: uPlot; calls: RecordedDrawCalls } {
  const calls: RecordedDrawCalls = {
    fillRects: [],
    moveTos: 0,
    lineTos: 0,
    strokes: 0,
    clips: 0,
    clipsUsed: 0,
  };
  const ctx = {
    strokeStyle: "",
    fillStyle: "",
    lineWidth: 1,
    lineJoin: "miter",
    lineCap: "butt",
    save: () => undefined,
    restore: () => undefined,
    beginPath: () => undefined,
    rect: () => {
      calls.clips++;
    },
    clip: () => {
      calls.clipsUsed++;
    },
    moveTo: () => {
      calls.moveTos++;
    },
    lineTo: () => {
      calls.lineTos++;
    },
    stroke: () => {
      calls.strokes++;
    },
    fillRect: (x: number, y: number, w: number, h: number) => {
      calls.fillRects.push({ x, y, w, h });
    },
  };
  const plot = {
    mode: 1,
    ctx,
    data: [xs, ys],
    bbox: { left: xOff, top: yOff, width: widthPx, height: heightPx },
    scales: {
      x: { ori: 0, min: xs[0], max: xs[xs.length - 1] },
      y: { ori: 1, min: 0, max: 100 },
    },
    series: [
      { scale: "x" },
      {
        scale: "y",
        show: true,
        width: 1.05,
        stroke: "#ff5500",
        idxs: [0, xs.length - 1],
      },
    ],
  } as unknown as uPlot;
  return { plot, calls };
}

describe("dsoPaths", () => {
  it("detects halts separating samples while bridging non-halt alignment gaps", () => {
    const halts: HaltSpan[] = [{ from: 5.0, to: 6.5 }];
    expect(hasHaltBetween(halts, 4.99, 6.51)).toBe(true);
    expect(hasHaltBetween(halts, 1.0, 2.0)).toBe(false);
    expect(hasHaltBetween(halts, 4.998, 5.0)).toBe(false);
  });

  it("returns null from nullPathBuilder so uPlot skips Path2D allocation", () => {
    const { plot } = makeFakePlot(new Float64Array([0, 1]), new Float64Array([10, 20]), 500, 200);
    expect(nullPathBuilder(plot, 1, 0, 1)).toBeNull();
  });

  it("draws sparse data as rectangular-clipped polylines broken across halts", () => {
    const xs = new Float64Array([0, 1, 2, 5, 6, 7]);
    const ys = new Float64Array([10, 20, NaN, 80, 90, 100]);
    const halts: HaltSpan[] = [{ from: 2.5, to: 4.5 }];
    const { plot, calls } = makeFakePlot(xs, ys, 600, 200);
    drawDsoSeries(plot, 1, halts);
    expect(calls.fillRects).toHaveLength(0);
    expect(calls.moveTos).toBe(2); // initial start + after halt break
    expect(calls.lineTos).toBe(3);
    expect(calls.strokes).toBe(1);
    expect(calls.clipsUsed).toBe(1);
  });

  it("renders dense oscillations with clamped ctx.fillRect columns and zero Path2D usage", () => {
    const prevPath2D = (globalThis as { Path2D?: unknown }).Path2D;
    (globalThis as { Path2D: unknown }).Path2D = function () {
      throw new Error("Path2D must not be constructed in drawDsoSeries");
    };
    try {
      const n = 2000;
      const xs = new Float64Array(n);
      const ys = new Float64Array(n);
      for (let i = 0; i < n; i++) {
        xs[i] = i * 0.001;
        // Include overshoot beyond [0, 100] to verify manual Y clamping without clip().
        ys[i] = 50 + 80 * Math.sin(i * 0.7);
      }
      const xOff = 56;
      const yOff = 12;
      const widthPx = 500;
      const heightPx = 200;
      const { plot, calls } = makeFakePlot(xs, ys, widthPx, heightPx, xOff, yOff);
      drawDsoSeries(plot, 1, []);
      expect(calls.clipsUsed).toBe(0);
      expect(calls.fillRects.length).toBeGreaterThanOrEqual(450);
      expect(calls.fillRects.length).toBeLessThanOrEqual(505);
      for (const r of calls.fillRects) {
        expect(r.x).toBeGreaterThanOrEqual(xOff);
        expect(r.x + r.w).toBeLessThanOrEqual(xOff + widthPx);
        expect(r.y).toBeGreaterThanOrEqual(yOff);
        expect(r.y + r.h).toBeLessThanOrEqual(yOff + heightPx);
        expect(r.w).toBeGreaterThan(0);
        expect(r.h).toBeGreaterThan(0);
      }
    } finally {
      if (prevPath2D === undefined) {
        delete (globalThis as { Path2D?: unknown }).Path2D;
      } else {
        (globalThis as { Path2D: unknown }).Path2D = prevPath2D;
      }
    }
  });
});
