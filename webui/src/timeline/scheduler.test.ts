import { describe, expect, it } from "vitest";
import { RenderScheduler, type SchedulerHost } from "./scheduler";

function createTestHarness() {
  const queue: FrameRequestCallback[] = [];
  let hidden = false;
  let followLive = false;
  let resolveQuery: (() => void) | null = null;
  const counts = {
    advanceLiveClock: 0,
    runQuery: 0,
    applyLayout: 0,
    redrawWaveforms: 0,
    redrawOverlays: 0,
    updateReadouts: 0,
    updateInspector: 0,
  };

  const host: SchedulerHost = {
    isHidden: () => hidden,
    shouldFollowLive: () => followLive,
    advanceLiveClock: () => {
      counts.advanceLiveClock++;
    },
    runQuery: () => {
      counts.runQuery++;
      return new Promise<void>((resolve) => {
        resolveQuery = resolve;
      });
    },
    applyLayout: () => {
      counts.applyLayout++;
    },
    redrawWaveforms: () => {
      counts.redrawWaveforms++;
    },
    redrawOverlays: () => {
      counts.redrawOverlays++;
    },
    updateReadouts: () => {
      counts.updateReadouts++;
    },
    updateInspector: () => {
      counts.updateInspector++;
    },
    requestFrame: (cb) => {
      queue.push(cb);
      return queue.length;
    },
  };

  const scheduler = new RenderScheduler(host);

  return {
    scheduler,
    counts,
    pendingFrames: () => queue.length,
    stepFrame: (now = 16.67) => {
      const next = queue.shift();
      if (next) next(now);
    },
    finishQuery: async () => {
      const done = resolveQuery;
      resolveQuery = null;
      done?.();
      await Promise.resolve();
      await Promise.resolve();
    },
    setHidden: (val: boolean) => {
      hidden = val;
    },
    setFollowLive: (val: boolean) => {
      followLive = val;
    },
  };
}

describe("RenderScheduler", () => {
  it("stays completely idle when paused and clean", () => {
    const h = createTestHarness();
    h.scheduler.wake();
    expect(h.pendingFrames()).toBe(0);
    expect(h.counts.runQuery).toBe(0);
  });

  it("coalesces multiple pointer and wheel invalidations into a single frame", async () => {
    const h = createTestHarness();
    for (let i = 0; i < 10; i++) {
      h.scheduler.invalidate("overlays", "readouts", "query");
    }
    expect(h.pendingFrames()).toBe(1);

    h.stepFrame();
    expect(h.counts.runQuery).toBe(1);
    expect(h.counts.redrawOverlays).toBe(1);
    expect(h.counts.updateReadouts).toBe(1);

    // Additional wheel events while query is in flight coalesce into one follow-up query
    h.scheduler.invalidate("query", "overlays");
    h.scheduler.invalidate("query", "overlays");
    h.stepFrame();
    expect(h.counts.runQuery).toBe(1);
    expect(h.counts.redrawOverlays).toBe(2);

    await h.finishQuery();
    expect(h.pendingFrames()).toBe(1);
    h.stepFrame();
    expect(h.counts.runQuery).toBe(2);

    await h.finishQuery();
    expect(h.pendingFrames()).toBe(0);
  });

  it("locks live following to VSync with natural backpressure and pauses when hidden", async () => {
    const h = createTestHarness();
    h.setFollowLive(true);
    h.scheduler.wake();
    expect(h.pendingFrames()).toBe(1);

    h.stepFrame();
    expect(h.counts.advanceLiveClock).toBe(1);
    expect(h.counts.runQuery).toBe(1);
    // Does not spin extra rAF frames while waiting for in-flight query
    expect(h.pendingFrames()).toBe(0);

    await h.finishQuery();
    // Schedules next VSync frame once query resolves
    expect(h.pendingFrames()).toBe(1);

    // Hiding document suspends frame loop until woken
    h.setHidden(true);
    h.stepFrame();
    expect(h.counts.runQuery).toBe(1);
    expect(h.pendingFrames()).toBe(0);

    h.setHidden(false);
    h.scheduler.wake();
    expect(h.pendingFrames()).toBe(1);
  });
});
