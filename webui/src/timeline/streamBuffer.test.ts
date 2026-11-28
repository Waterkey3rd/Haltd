import { describe, expect, it } from "vitest";
import { parseSamplesFrame, type QueryResult, type SeriesInfo } from "../client";
import { LiveClockPll, StreamBuffer } from "./streamBuffer";

function buildKind1Frame(
  blocks: Array<{ seriesId: number; tsNs: bigint[]; vs: number[] }>,
): ArrayBuffer {
  let totalBytes = 4;
  for (const b of blocks) {
    totalBytes += 20 + b.tsNs.length * 16;
  }
  const buf = new ArrayBuffer(totalBytes);
  const view = new DataView(buf);
  view.setUint8(0, 1); // KIND_SAMPLES
  view.setUint8(1, 1); // VERSION
  view.setUint16(2, blocks.length, true);
  let offset = 4;
  for (const b of blocks) {
    const n = b.tsNs.length;
    view.setUint32(offset, b.seriesId, true);
    view.setUint32(offset + 4, n, true);
    view.setBigInt64(offset + 8, n > 0 ? b.tsNs[0] : 0n, true);
    view.setUint32(offset + 16, 0, true);
    const dataAt = offset + 20;
    for (let i = 0; i < n; i++) {
      view.setBigInt64(dataAt + i * 8, b.tsNs[i], true);
      view.setFloat64(dataAt + n * 8 + i * 8, b.vs[i], true);
    }
    offset = dataAt + n * 16;
  }
  return buf;
}

function makeSeries(seriesId: number, name: string): SeriesInfo {
  return {
    seriesId,
    name,
    root: name,
    mode: "hss",
    actualMode: "hss",
    rateHz: 1000,
    error: null,
    group: null,
    color: null,
  };
}

describe("parseSamplesFrame", () => {
  it("decodes multiple blocks including unaligned offsets after odd counts", () => {
    const buf = buildKind1Frame([
      {
        seriesId: 1,
        tsNs: [1_000_000_000n, 1_001_000_000n, 1_002_000_000n],
        vs: [10.5, 20.5, 30.5],
      },
      {
        seriesId: 2,
        tsNs: [1_000_000_000n, 1_001_000_000n],
        vs: [-1.25, -2.5],
      },
    ]);
    const batches = parseSamplesFrame(buf);
    expect(batches).toHaveLength(2);
    expect(batches[0].seriesId).toBe(1);
    expect(Array.from(batches[0].ts)).toEqual([1.0, 1.001, 1.002]);
    expect(Array.from(batches[0].vs)).toEqual([10.5, 20.5, 30.5]);
    expect(batches[1].seriesId).toBe(2);
    expect(Array.from(batches[1].ts)).toEqual([1.0, 1.001]);
    expect(Array.from(batches[1].vs)).toEqual([-1.25, -2.5]);
  });
});

describe("StreamBuffer", () => {
  it("appends batches, seeds earlier raw history without duplicates, and binary-searches values", () => {
    const buffer = new StreamBuffer();
    buffer.appendBatches([
      {
        seriesId: 1,
        ts: new Float64Array([1.003, 1.004, 1.005]),
        vs: new Float64Array([30, 40, 50]),
      },
    ]);

    const rawResult: QueryResult = {
      raw: true,
      revision: 10,
      t0: 1_000_000_000,
      t1: 1_004_000_000,
      columns: 4,
      rawXs: new Float64Array([1.0, 1.001, 1.002, 1.003]),
      series: new Map([
        [
          1,
          {
            min: new Float64Array([0, 10, 20, 30]),
            max: new Float64Array([0, 10, 20, 30]),
          },
        ],
      ]),
    };
    buffer.seedFromRawResult(rawResult, [1, 2]);
    expect(buffer.canServe([1, 2], 1.0, 1.0)).toBe(true);
    expect(buffer.canServe([1, 2], 0.5, 0.5)).toBe(false);
    expect(buffer.valueAt(1, 1.0012)).toBe(10);
    expect(buffer.valueAt(1, 1.0049)).toBe(50);
    expect(buffer.lastValue(1)).toBe(50);
    expect(buffer.lastValue(2)).toBeNull();
  });

  it("handles ring growth past initial capacity and serves streamed spans without raw seed", () => {
    const buffer = new StreamBuffer();
    const n = 5000;
    const ts = new Float64Array(n);
    const vs = new Float64Array(n);
    for (let i = 0; i < n; i++) {
      ts[i] = 50 + i * 0.001;
      vs[i] = i;
    }
    buffer.appendBatches([{ seriesId: 7, ts, vs }]);
    expect(buffer.canServe([7], 51, 0)).toBe(true);
    expect(buffer.canServe([7], 40, 0)).toBe(false);
    expect(buffer.valueAt(7, 50)).toBe(0);
    expect(buffer.valueAt(7, 54.999)).toBe(4999);
    expect(buffer.lastValue(7)).toBe(4999);
  });

  it("aligns shared raw waveforms and merges mixed-rate series using NaN Float64Arrays", () => {
    const buffer = new StreamBuffer();
    const s1 = makeSeries(1, "a");
    const s2 = makeSeries(2, "b");
    buffer.appendBatches([
      {
        seriesId: 1,
        ts: new Float64Array([1.0, 1.001, 1.02, 1.021]),
        vs: new Float64Array([10, 11, 20, 21]),
      },
      {
        seriesId: 2,
        ts: new Float64Array([1.0, 1.001, 1.02, 1.021]),
        vs: new Float64Array([100, 101, 200, 201]),
      },
    ]);

    const shared = buffer.buildAlignedTrackData(
      [s1, s2],
      [true, true],
      0.99,
      1.03,
      1000,
      1,
      [],
      false,
      () => "#4dabf7",
      "24",
    );
    expect(shared.isRaw).toBe(true);
    expect(Array.from(shared.data[0])).toEqual([1.0, 1.001, 1.02, 1.021]);
    expect(Array.from(shared.data[1])).toEqual([10, 11, 20, 21]);
    expect(shared.seriesShow).toEqual([true, true]);

    const s3 = makeSeries(3, "slow");
    buffer.appendBatches([
      {
        seriesId: 3,
        ts: new Float64Array([1.0005, 1.0205]),
        vs: new Float64Array([5, 6]),
      },
    ]);
    const mixed = buffer.buildAlignedTrackData(
      [s1, s3],
      [true, true],
      0.99,
      1.03,
      1000,
      1,
      [],
      false,
      () => "#4dabf7",
      "24",
    );
    expect(mixed.data[0].length).toBe(6);
    expect(Number.isNaN(mixed.data[2][0] as number)).toBe(true);
    expect(mixed.data[2][1]).toBe(5);
  });

  it("uses O(log N) min/max pyramid for 4 series x 600k samples (30s/div) in < 10 ms", () => {
    const buffer = new StreamBuffer();
    const seriesList = [1, 2, 3, 4].map((id) => makeSeries(id, `sig_${id}`));
    const total = 600_000;
    const ts = new Float64Array(total);
    const vs = new Float64Array(total);
    for (let i = 0; i < total; i++) {
      ts[i] = i * 0.001;
      vs[i] = Math.sin(i * 0.17) * 1000 + i * 0.001;
    }
    buffer.appendBatches(
      seriesList.map((s) => ({
        seriesId: s.seriesId,
        ts,
        vs,
      })),
    );
    expect(buffer.canServe([1, 2, 3, 4], 300, 0)).toBe(true);

    // Warm-up call once to allocate reusable track scratch buffers.
    buffer.buildAlignedTrackData(
      seriesList,
      [true, true, true, true],
      300,
      600,
      1800,
      1,
      [],
      false,
      () => "#4dabf7",
      "24",
      "default",
    );

    const started = performance.now();
    const built = buffer.buildAlignedTrackData(
      seriesList,
      [true, true, true, true],
      300.003,
      600.003,
      1800,
      1,
      [],
      false,
      () => "#4dabf7",
      "24",
      "default",
    );
    const elapsedMs = performance.now() - started;

    expect(built.isRaw).toBe(false);
    expect(built.data.length).toBe(5); // 1 X + 4 Y series
    expect(elapsedMs).toBeLessThan(10);
  });
});

describe("LiveClockPll", () => {
  it("advances smoothly with VSync frames and clamps behind HSS tip without stepping", () => {
    const pll = new LiveClockPll();
    const v0 = pll.step(10.0, 1000, 9.97, true);
    expect(v0).toBeCloseTo(9.96, 3);
    const v1 = pll.step(10.016, 1016, 9.985, true);
    const v2 = pll.step(10.032, 1032, 10.0, true);
    expect(v1).toBeGreaterThan(v0);
    expect(v2).toBeGreaterThan(v1);
    expect(v2 - v1).toBeCloseTo(0.016, 2);
  });
});
