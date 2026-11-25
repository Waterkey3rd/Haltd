export type DirtyFlag =
  | "layout"
  | "query"
  | "waveforms"
  | "overlays"
  | "readouts"
  | "inspector";

export interface SchedulerHost {
  isHidden: () => boolean;
  shouldFollowLive: () => boolean;
  advanceLiveClock: () => void;
  runQuery: () => Promise<void>;
  applyLayout: () => void;
  redrawWaveforms: () => void;
  redrawOverlays: () => void;
  updateReadouts: () => void;
  updateInspector: () => void;
  requestFrame?: (cb: FrameRequestCallback) => number;
}

/**
 * Single-point VSync frame coordinator for Timeline rendering.
 *
 * - Locks live-follow updates to display VSync (60/120 Hz) with natural backpressure
 *   instead of a fixed 33 ms timer beating against 16.67 ms frames.
 * - Coalesces bursty pointer/wheel/scrub inputs into at most one in-flight query and
 *   one paint per animation frame.
 * - Idles at zero CPU and zero network traffic when paused or hidden.
 */
export class RenderScheduler {
  private readonly dirty = new Set<DirtyFlag>();
  private readonly requestFrame: (cb: FrameRequestCallback) => number;
  private rafPending = false;
  private queryInFlight = false;

  constructor(private readonly host: SchedulerHost) {
    this.requestFrame =
      host.requestFrame ?? ((cb: FrameRequestCallback) => requestAnimationFrame(cb));
  }

  invalidate(...flags: DirtyFlag[]): void {
    for (const flag of flags) {
      this.dirty.add(flag);
    }
    this.ensureScheduled();
  }

  /** Re-evaluates scheduling state (e.g. when tab becomes visible or followLive toggles). */
  wake(): void {
    this.ensureScheduled();
  }

  isQueryInFlight(): boolean {
    return this.queryInFlight;
  }

  private ensureScheduled(): void {
    if (this.rafPending || this.host.isHidden()) {
      return;
    }
    if (this.dirty.size === 0 && (!this.host.shouldFollowLive() || this.queryInFlight)) {
      return;
    }
    this.rafPending = true;
    this.requestFrame(this.tick);
  }

  private readonly tick = (): void => {
    this.rafPending = false;
    if (this.host.isHidden()) {
      return;
    }

    if (this.dirty.has("layout")) {
      this.dirty.delete("layout");
      this.host.applyLayout();
    }

    const live = this.host.shouldFollowLive();
    const wantQuery = this.dirty.has("query") || live;
    if (wantQuery && !this.queryInFlight) {
      this.dirty.delete("query");
      if (live) {
        this.host.advanceLiveClock();
      }
      this.queryInFlight = true;
      void this.host.runQuery().finally(() => {
        this.queryInFlight = false;
        if (this.dirty.size > 0 || this.host.shouldFollowLive()) {
          this.ensureScheduled();
        }
      });
    }

    if (this.dirty.has("waveforms")) {
      this.dirty.delete("waveforms");
      this.host.redrawWaveforms();
    }
    if (this.dirty.has("overlays")) {
      this.dirty.delete("overlays");
      this.host.redrawOverlays();
    }
    if (this.dirty.has("readouts")) {
      this.dirty.delete("readouts");
      this.host.updateReadouts();
    }
    if (this.dirty.has("inspector")) {
      this.dirty.delete("inspector");
      this.host.updateInspector();
    }
  };
}
