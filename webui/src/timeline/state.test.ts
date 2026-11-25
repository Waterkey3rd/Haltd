import { describe, expect, it } from "vitest";
import { addPin, clearAllPins, removePin, type Pin } from "./state";

describe("timeline pin state helpers", () => {
  it("increments pin IDs monotonically even after removal", () => {
    const state: { pins: Pin[]; nextPinId: number } = { pins: [], nextPinId: 1 };

    const p1 = addPin(state, 1.0);
    const p2 = addPin(state, 2.5);
    expect(p1).toEqual({ id: 1, t: 1.0, label: "M1" });
    expect(p2).toEqual({ id: 2, t: 2.5, label: "M2" });
    expect(state.pins).toHaveLength(2);

    expect(removePin(state, 1)).toBe(true);
    expect(removePin(state, 999)).toBe(false);
    expect(state.pins).toEqual([{ id: 2, t: 2.5, label: "M2" }]);

    const p3 = addPin(state, 4.0);
    expect(p3).toEqual({ id: 3, t: 4.0, label: "M3" });
    expect(state.pins).toHaveLength(2);
  });

  it("clears all pins and temporary sample cursor together", () => {
    const state = {
      pins: [] as Pin[],
      nextPinId: 1,
      sampleCursor: 1.25 as number | null,
      sampleCursorPinned: true,
    };
    addPin(state, 0.5);
    addPin(state, 1.5);

    clearAllPins(state);
    expect(state.pins).toEqual([]);
    expect(state.nextPinId).toBe(1);
    expect(state.sampleCursor).toBeNull();
    expect(state.sampleCursorPinned).toBe(false);

    const next = addPin(state, 3.0);
    expect(next).toEqual({ id: 1, t: 3.0, label: "M1" });
  });
});
