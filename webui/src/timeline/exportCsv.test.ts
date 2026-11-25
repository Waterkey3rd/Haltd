import { describe, expect, it } from "vitest";
import type { CanvasTheme } from "../theme";
import {
  buildExportCsvUrl,
  buildRecordingHlrecUrl,
  defaultExportFilename,
  defaultExportRangeMode,
  resolveTimeBoundsNs,
} from "./exportCsv";
import { createInitialState } from "./state";

const TEST_THEME: CanvasTheme = {
  kind: "dark",
  isLight: false,
  isHighContrast: false,
  bg: "#181a1f",
  plotBg: "#14161a",
  axisStroke: "#dcdfe4",
  gridMajor: "#2a2f3a",
  gridMinor: "#20242c",
  tickStroke: "#8b929e",
  crosshair: "#6c7583",
  sampleCursor: "#fcc419",
  pinColor: "#4dabf7",
  cursorA: "#4dabf7",
  cursorB: "#ff8787",
  cursorSpanFill: "rgba(77,171,247,0.1)",
  hoverBoxBg: "#20232a",
  hoverBoxBorder: "#3e4451",
  hoverBoxText: "#dcdfe4",
  haltFill: "rgba(255,107,107,0.15)",
  haltStroke: "#ff6b6b",
  faultColor: "#ff6b6b",
  resetColor: "#ffd43b",
  reloadColor: "#51cf66",
  samplingColor: "#4dabf7",
  selectionFill: "rgba(77,171,247,0.15)",
  selectionStroke: "#4dabf7",
  badgeText: "#ffffff",
  palette: ["#4dabf7", "#51cf66", "#ff8787"],
};

describe("timeline exportCsv helpers", () => {
  it("chooses cursor range when both cursors are set, otherwise visible window", () => {
    const state = createInitialState(TEST_THEME);
    expect(defaultExportRangeMode(state)).toBe("visible");

    state.cursorA = 1.5;
    expect(defaultExportRangeMode(state)).toBe("visible");

    state.cursorB = 0.75;
    expect(defaultExportRangeMode(state)).toBe("cursors");
  });

  it("computes integer nanosecond bounds for cursors, visible window, and full history", () => {
    const state = createInitialState(TEST_THEME);
    state.division = 0.5; // 5s span
    state.followLive = false;
    state.viewEnd = 12.0;
    state.cursorA = 4.2;
    state.cursorB = 2.0;

    expect(resolveTimeBoundsNs("cursors", state, 20)).toEqual({
      t0: 2_000_000_000,
      t1: 4_200_000_000,
    });
    expect(resolveTimeBoundsNs("visible", state, 20)).toEqual({
      t0: 7_000_000_000,
      t1: 12_000_000_000,
    });

    state.followLive = true;
    expect(resolveTimeBoundsNs("visible", state, 20)).toEqual({
      t0: 15_000_000_000,
      t1: 20_000_000_000,
    });

    expect(resolveTimeBoundsNs("all", state, 20)).toEqual({});
    expect(resolveTimeBoundsNs("recording", state, 20)).toEqual({});
  });

  it("builds semantic export filenames and URLs", () => {
    const fixedDate = new Date(2026, 3, 5, 9, 8, 7); // 20260405_090807
    expect(defaultExportFilename("FOC: Current / Loop", "csv", fixedDate)).toBe(
      "FOC_Current_Loop_20260405_090807.csv",
    );
    expect(defaultExportFilename(null, "hlrec", fixedDate)).toBe(
      "timeline_20260405_090807.hlrec",
    );

    const url = buildExportCsvUrl(
      {
        seriesIds: [1, 3],
        t0: 1000,
        t1: 2000,
        zoh: true,
        relativeTime: true,
      },
      "foc.csv",
    );
    expect(url).toContain("/api/export.csv?");
    expect(url).toContain("seriesIds=1%2C3");
    expect(url).toContain("zoh=true");
    expect(url).toContain("filename=foc.csv");

    expect(buildRecordingHlrecUrl("/tmp/rec.hlrec", "cap.hlrec")).toBe(
      "/api/recording.hlrec?hlrecPath=%2Ftmp%2Frec.hlrec&filename=cap.hlrec",
    );
  });
});
