import { describe, expect, it } from "vitest";
import {
  DIVISIONS,
  computeSeriesStatsFromColumns,
  divisionLabels,
  divisionSplits,
  fitTimeRange,
  formatFrequency,
  formatNumber,
  formatSeconds,
  panViewEnd,
  panYRange,
  pxToTime,
  spanSeconds,
  stepDivision,
  timeToPx,
  valueAtColumns,
  zoomDivisionAround,
} from "./math";

describe("timeline math", () => {
  it("steps divisions within bounds", () => {
    expect(stepDivision(0.5, -1)).toBe(0.2);
    expect(stepDivision(0.5, 1)).toBe(1);
    expect(stepDivision(DIVISIONS[0], -3)).toBe(DIVISIONS[0]);
    expect(stepDivision(DIVISIONS[DIVISIONS.length - 1], 5)).toBe(DIVISIONS[DIVISIONS.length - 1]);
  });

  it("zooms around the hovered time coordinate", () => {
    const startDiv = 1; // span = 10s
    const startEnd = 20; // [10, 20]
    const fraction = 0.25; // pivot = 12.5s
    const pivotBefore = startEnd - spanSeconds(startDiv) * (1 - fraction);
    const zoomed = zoomDivisionAround(startDiv, startEnd, -1, fraction);
    expect(zoomed.division).toBe(0.5); // span = 5s
    const pivotAfter = zoomed.viewEnd - spanSeconds(zoomed.division) * (1 - fraction);
    expect(pivotAfter).toBeCloseTo(pivotBefore, 6);
  });

  it("pans and clamps viewEnd", () => {
    expect(panViewEnd(10, -2.5, 0, 15)).toBe(7.5);
    expect(panViewEnd(10, -20, 0, 15)).toBe(0);
    expect(panViewEnd(10, 20, 0, 15)).toBe(15);
  });

  it("pans Y range up and down while preserving span", () => {
    // Dragging mouse down (+50px out of 200px) moves viewport up so waveform follows cursor down.
    const down = panYRange(-10, 10, 50, 200);
    expect(down.min).toBeCloseTo(-5, 6);
    expect(down.max).toBeCloseTo(15, 6);
    expect(down.max - down.min).toBeCloseTo(20, 6);

    const up = panYRange(-10, 10, -25, 200);
    expect(up.min).toBeCloseTo(-12.5, 6);
    expect(up.max).toBeCloseTo(7.5, 6);
  });

  it("fits a time range into the nearest division without clipping below zero", () => {
    const fit1 = fitTimeRange(0, 3.2);
    expect(fit1.division).toBe(0.5); // 5s span fits 3.2s
    expect(fit1.viewEnd).toBeGreaterThanOrEqual(3.2);
    expect(fit1.viewEnd - spanSeconds(fit1.division)).toBeGreaterThanOrEqual(0);

    const fit2 = fitTimeRange(12, 13.5);
    expect(fit2.division).toBe(0.2); // 2s span fits 1.5s
    expect(fit2.viewEnd).toBeCloseTo(13.75, 6);
  });

  it("converts pixels and times inversely", () => {
    const t = pxToTime(150, 600, 10, 14);
    expect(t).toBeCloseTo(11, 6);
    expect(timeToPx(t, 600, 10, 14)).toBeCloseTo(150, 6);
  });

  it("computes aligned division splits and formatted labels", () => {
    const splits = divisionSplits(0.5, 1.0, 2.5);
    expect(splits).toEqual([1, 1.5, 2, 2.5]);
    expect(divisionLabels(0.5, splits)).toEqual(["1.0", "1.5", "2.0", "2.5"]);
  });

  it("formats numbers, durations and frequencies", () => {
    expect(formatNumber(null)).toBe("—");
    expect(formatNumber(0)).toBe("0");
    expect(formatNumber(12.3456, 4)).toBe("12.35");
    expect(formatSeconds(1.25)).toBe("1.250 s");
    expect(formatSeconds(0.0045)).toBe("4.500 ms");
    expect(formatSeconds(0.00025)).toBe("250.0 µs");
    expect(formatFrequency(0.001)).toBe("1.00 kHz");
  });

  it("interpolates nearest non-NaN column value and computes series stats", () => {
    const xs = [0, 1, 2, 3];
    const data = {
      min: new Float64Array([1, NaN, 4, 2]),
      max: new Float64Array([3, NaN, 6, 4]),
    };
    expect(valueAtColumns(xs, data, 0)).toBe(2);
    expect(valueAtColumns(xs, data, 2.1)).toBe(5);
    expect(valueAtColumns(xs, data, 1)).not.toBeNull();
    expect(computeSeriesStatsFromColumns(data)).toEqual({
      last: 3,
      min: 1,
      max: 6,
      avg: (2 + 5 + 3) / 3,
      pp: 5,
    });
  });
});
