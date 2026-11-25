import { describe, expect, it } from "vitest";
import type { SeriesInfo } from "../client";
import type { CanvasTheme } from "../theme";
import {
  buildPresetFromState,
  isPresetDirty,
  presetSignature,
  type TimelinePreset,
} from "./presets";
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

function makeSeries(overrides: Partial<SeriesInfo>): SeriesInfo {
  return {
    seriesId: 1,
    name: "g_speed",
    root: "g_speed",
    mode: "auto",
    actualMode: "hss",
    rateHz: 1000,
    error: null,
    group: null,
    color: null,
    ...overrides,
  };
}

describe("timeline presets helpers", () => {
  it("captures signals, visibility, tracks, and RTT modes into a TimelinePreset", () => {
    const state = createInitialState(TEST_THEME);
    state.division = 0.2;
    state.splitLanes = false;
    state.showBands = false;
    state.series = [
      makeSeries({
        seriesId: 10,
        name: "g_motor.iq",
        group: "FOC",
        color: "#ff5500",
      }),
      makeSeries({
        seriesId: 11,
        name: "rtt:imu_z",
        mode: "rtt",
        actualMode: "rtt",
        rateHz: 0,
        group: "IMU",
        color: "#00bcd4",
      }),
    ];
    state.hidden.add(11);
    state.groupYScales.set("FOC", { auto: false, min: -10, max: 10, collapsed: true });
    state.trackHeights.set("FOC", 220);

    const preset = buildPresetFromState("FOC Debug", state, 1000);
    expect(preset.name).toBe("FOC Debug");
    expect(preset.rateHz).toBe(1000);
    expect(preset.division).toBe(0.2);
    expect(preset.splitLanes).toBe(false);
    expect(preset.showBands).toBe(false);
    expect(preset.signals).toEqual([
      {
        kind: "expr",
        expr: "g_motor.iq",
        mode: "auto",
        group: "FOC",
        color: "#ff5500",
        hidden: false,
      },
      {
        kind: "rtt",
        expr: "rtt:imu_z",
        mode: "rtt",
        group: "IMU",
        color: "#00bcd4",
        hidden: true,
      },
    ]);
    expect(preset.tracks.FOC).toEqual({
      autoY: false,
      yMin: -10,
      yMax: 10,
      heightPx: 220,
      collapsed: true,
    });
  });

  it("ignores preset name and division changes when computing dirty state", () => {
    const state = createInitialState(TEST_THEME);
    state.series = [makeSeries({ seriesId: 1, name: "g_a", group: "default" })];

    const base: TimelinePreset = buildPresetFromState("Preset A", state, 500);
    state.activePresetName = "Preset A";
    state.savedPresetSnapshot = presetSignature(base);

    expect(isPresetDirty(state, 500)).toBe(false);

    // Zooming timebase should not mark preset dirty.
    state.division = 2;
    expect(isPresetDirty(state, 500)).toBe(false);

    // Changing sampling rate or toggling split lanes or adding a signal should mark preset dirty.
    expect(isPresetDirty(state, 200)).toBe(true);

    state.splitLanes = true;
    expect(isPresetDirty(state, 500)).toBe(true);
    state.splitLanes = false;

    state.series.push(makeSeries({ seriesId: 2, name: "g_b", group: "default" }));
    expect(isPresetDirty(state, 500)).toBe(true);
  });
});
