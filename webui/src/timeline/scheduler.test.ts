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

  it("paces live-follow queries to minLiveIntervalMs while keeping overlays unthrottled", async () => {
    let fakeNow = 0;
    const queue: FrameRequestCallback[] = [];
    let followLive = true;
    const pendingDone: Array<() => void> = [];
    const counts = { runQuery: 0, redrawOverlays: 0 };
    const finishQuery = async () => {
      pendingDone.shift()?.();
      await Promise.resolve();
      await Promise.resolve();
    };

    const scheduler = new RenderScheduler({
      isHidden: () => false,
      shouldFollowLive: () => followLive,
      advanceLiveClock: () => {},
      runQuery: () => {
        counts.runQuery++;
        return new Promise<void>((r) => {
          pendingDone.push(r);
        });
      },
      applyLayout: () => {},
      redrawWaveforms: () => {},
      redrawOverlays: () => {
        counts.redrawOverlays++;
      },
      updateReadouts: () => {},
      updateInspector: () => {},
      minLiveIntervalMs: 22,
      now: () => fakeNow,
      requestFrame: (cb) => {
        queue.push(cb);
        return queue.length;
      },
    });

    scheduler.wake();
    queue.shift()?.(0);
    expect(counts.runQuery).toBe(1);
    await finishQuery();

    // 6 ms later (like a 165 Hz frame): overlays still run when invalidated, query waits.
    fakeNow = 6;
    scheduler.invalidate("overlays");
    queue.shift()?.(6);
    expect(counts.redrawOverlays).toBe(1);
    expect(counts.runQuery).toBe(1);

    // Explicit query (e.g. user zoom/pan) bypasses live pacing immediately.
    fakeNow = 10;
    scheduler.invalidate("query");
    queue.shift()?.(10);
    expect(counts.runQuery).toBe(2);
    await finishQuery();

    // Once minLiveIntervalMs elapses cleanly, live follow runs again.
    followLive = true;
    fakeNow = 40;
    scheduler.invalidate("overlays");
    queue.shift()?.(40);
    expect(counts.runQuery).toBe(3);
  });

  it("renders synchronous stream frames on VSync without issuing async RPC queries", () => {
    const queue: FrameRequestCallback[] = [];
    let streamReady = true;
    let streamFrames = 0;
    let rpcQueries = 0;

    const scheduler = new RenderScheduler({
      isHidden: () => false,
      shouldFollowLive: () => true,
      advanceLiveClock: () => {},
      renderLiveStream: () => {
        if (!streamReady) return false;
        streamFrames++;
        return true;
      },
      runQuery: () => {
        rpcQueries++;
        return Promise.resolve();
      },
      applyLayout: () => {},
      redrawWaveforms: () => {},
      redrawOverlays: () => {},
      updateReadouts: () => {},
      updateInspector: () => {},
      requestFrame: (cb) => {
        queue.push(cb);
        return queue.length;
      },
    });

    scheduler.wake();
    expect(queue).toHaveLength(1);
    queue.shift()?.(16.6);
    expect(streamFrames).toBe(1);
    expect(rpcQueries).toBe(0);
    expect(queue).toHaveLength(1); // Next VSync scheduled immediately

    streamReady = false;
    queue.shift()?.(33.3);
    expect(rpcQueries).toBe(1);
  });
});
