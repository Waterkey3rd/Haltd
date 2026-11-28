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
  advanceLiveClock: (frameTimeMs?: number) => void;
  renderLiveStream?: () => boolean;
  runQuery: () => Promise<void>;
  applyLayout: () => void;
  redrawWaveforms: () => void;
  redrawOverlays: () => void;
  updateReadouts: () => void;
  updateInspector: () => void;
  requestFrame?: (cb: FrameRequestCallback) => number;
  minLiveIntervalMs?: number;
  getMinLiveIntervalMs?: () => number;
  now?: () => number;
}

/**
 * Single-point VSync frame coordinator for Timeline rendering.
 *
 * - Locks live-follow updates to display VSync (or capped `minLiveIntervalMs` for
 *   expensive waveform queries) with natural backpressure while keeping overlays at
 *   full display rate.
 * - Coalesces bursty pointer/wheel/scrub inputs into at most one in-flight query and
 *   one paint per animation frame.
 * - Idles at zero CPU and zero network traffic when paused or hidden.
 */
export class RenderScheduler {
  private readonly dirty = new Set<DirtyFlag>();
  private readonly requestFrame: (cb: FrameRequestCallback) => number;
  private readonly minLiveIntervalMs: number;
  private readonly now: () => number;
  private rafPending = false;
  private queryInFlight = false;
  private lastLiveQueryAt = -Infinity;
  private paceTimer: ReturnType<typeof setTimeout> | null = null;

  constructor(private readonly host: SchedulerHost) {
    this.requestFrame =
      host.requestFrame ?? ((cb: FrameRequestCallback) => requestAnimationFrame(cb));
    this.minLiveIntervalMs = host.minLiveIntervalMs ?? 0;
    this.now = host.now ?? (() => performance.now());
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

  private clearPaceTimer(): void {
    if (this.paceTimer !== null) {
      clearTimeout(this.paceTimer);
      this.paceTimer = null;
    }
  }

  private effectiveMinLiveIntervalMs(): number {
    return this.host.getMinLiveIntervalMs
      ? this.host.getMinLiveIntervalMs()
      : this.minLiveIntervalMs;
  }

  private schedulePaceWake(delayMs: number): void {
    if (this.paceTimer !== null) {
      return;
    }
    this.paceTimer = setTimeout(() => {
      this.paceTimer = null;
      this.ensureScheduled();
    }, Math.max(1, Math.ceil(delayMs)));
  }

  private ensureScheduled(): void {
    if (this.rafPending || this.host.isHidden()) {
      return;
    }
    const live = this.host.shouldFollowLive();
    if (this.dirty.size === 0 && !live) {
      return;
    }
    if (this.dirty.size === 0 && live) {
      if (this.queryInFlight) {
        return;
      }
      const minInterval = this.effectiveMinLiveIntervalMs();
      if (minInterval > 0) {
        const remaining = minInterval - (this.now() - this.lastLiveQueryAt);
        if (remaining > 0) {
          this.schedulePaceWake(remaining);
          return;
        }
      }
    }
    this.rafPending = true;
    this.requestFrame(this.tick);
  }

  private readonly tick = (frameTimeMs: number): void => {
    this.rafPending = false;
    if (this.host.isHidden()) {
      return;
    }

    if (this.dirty.has("layout")) {
      this.dirty.delete("layout");
      this.host.applyLayout();
    }

    const live = this.host.shouldFollowLive();
    const explicitQuery = this.dirty.has("query");
    const wantQuery = explicitQuery || live;
    let renderedByStream = false;

    if (wantQuery && live) {
      if (this.host.renderLiveStream) {
        this.host.advanceLiveClock(frameTimeMs);
        if (this.host.renderLiveStream()) {
          renderedByStream = true;
          this.clearPaceTimer();
          this.lastLiveQueryAt = this.now();
          this.dirty.delete("query");
          this.dirty.delete("waveforms");
          this.dirty.delete("overlays");
          this.dirty.delete("readouts");
          this.dirty.delete("inspector");
        }
      }
    }

    if (wantQuery && !renderedByStream && !this.queryInFlight) {
      let runNow = true;
      const minInterval = this.effectiveMinLiveIntervalMs();
      if (!explicitQuery && live && minInterval > 0) {
        const remaining = minInterval - (this.now() - this.lastLiveQueryAt);
        if (remaining > 0) {
          runNow = false;
          this.schedulePaceWake(remaining);
        }
      }
      if (runNow) {
        this.clearPaceTimer();
        this.dirty.delete("query");
        if (live && !this.host.renderLiveStream) {
          this.lastLiveQueryAt = this.now();
          this.host.advanceLiveClock(frameTimeMs);
        } else if (live) {
          this.lastLiveQueryAt = this.now();
        }
        this.queryInFlight = true;
        void this.host.runQuery().finally(() => {
          this.queryInFlight = false;
          if (this.dirty.size > 0 || this.host.shouldFollowLive()) {
            this.ensureScheduled();
          }
        });
      }
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

    if (renderedByStream && (this.dirty.size > 0 || this.host.shouldFollowLive())) {
      this.ensureScheduled();
    }
  };
}
