import { eventKindInfo } from "./cursors";
import { FOLLOW_LAG_S, spanSeconds } from "./math";
import { nowSeconds, type TimelineState } from "./state";

export function drawOverview(state: TimelineState): void {
  const canvas = document.getElementById("overview-canvas") as HTMLCanvasElement | null;
  const rangeEl = document.getElementById("overview-range");
  if (!canvas) return;
  const rect = canvas.getBoundingClientRect();
  if (rect.width <= 0 || rect.height <= 0) return;

  const dpr = devicePixelRatio || 1;
  const w = Math.round(rect.width * dpr);
  const h = Math.round(rect.height * dpr);
  if (canvas.width !== w || canvas.height !== h) {
    canvas.width = w;
    canvas.height = h;
  }
  const ctx = canvas.getContext("2d");
  if (!ctx) return;

  const theme = state.canvasTheme;
  ctx.clearRect(0, 0, w, h);

  const now = Math.max(nowSeconds(state), state.viewEnd, 1);
  const span = spanSeconds(state.division);
  const start = Math.min(state.historyStart, Math.max(0, state.viewEnd - span));
  const totalSpan = Math.max(1, now - start);
  const toX = (t: number) => ((t - start) / totalSpan) * w;

  if (state.recording && state.recordingStartedAt !== null) {
    const rx0 = Math.max(0, toX(state.recordingStartedAt));
    const rx1 = Math.min(w, toX(now));
    if (rx1 > rx0) {
      ctx.fillStyle = "rgba(255, 85, 85, 0.22)";
      ctx.fillRect(rx0, 0, rx1 - rx0, h);
      ctx.fillStyle = "#ff6b6b";
      ctx.fillRect(rx0, 0, rx1 - rx0, Math.max(1.5, dpr * 1.5));
    }
  } else if (
    state.lastRecording &&
    state.lastRecording.startedAt !== null &&
    state.lastRecording.endedAt !== null
  ) {
    const rx0 = Math.max(0, toX(state.lastRecording.startedAt));
    const rx1 = Math.min(w, toX(state.lastRecording.endedAt));
    if (rx1 > rx0) {
      ctx.fillStyle = "rgba(255, 85, 85, 0.14)";
      ctx.fillRect(rx0, 0, rx1 - rx0, h);
      ctx.fillStyle = "rgba(255, 107, 107, 0.7)";
      ctx.fillRect(rx0, 0, rx1 - rx0, Math.max(1.5, dpr * 1.5));
    }
  }

  ctx.fillStyle = theme.haltFill;
  for (const halt of state.halts) {
    const x0 = Math.max(0, toX(halt.from));
    const x1 = Math.min(w, toX(halt.to ?? now));
    if (x1 > x0) ctx.fillRect(x0, 0, x1 - x0, h);
  }

  for (const m of state.markers) {
    const mx = toX(m.t);
    if (mx >= 0 && mx <= w) {
      ctx.fillStyle = eventKindInfo(state, m.kind).color;
      ctx.fillRect(mx - dpr * 0.5, 0, Math.max(1.5, dpr), h);
    }
  }

  if (state.sampleCursor !== null) {
    const sx = toX(state.sampleCursor);
    ctx.fillStyle = theme.sampleCursor;
    ctx.fillRect(sx - dpr * 0.5, 0, Math.max(1.5, dpr), h);
  }
  ctx.fillStyle = theme.pinColor;
  for (const pin of state.pins) {
    const px = toX(pin.t);
    ctx.fillRect(px - dpr * 0.5, 0, Math.max(1.5, dpr), h);
  }
  if (state.cursorA !== null) {
    const ax = toX(state.cursorA);
    ctx.fillStyle = theme.cursorA;
    ctx.fillRect(ax - dpr * 0.5, 0, Math.max(1.5, dpr), h);
  }
  if (state.cursorB !== null) {
    const bx = toX(state.cursorB);
    ctx.fillStyle = theme.cursorB;
    ctx.fillRect(bx - dpr * 0.5, 0, Math.max(1.5, dpr), h);
  }

  const vEnd = state.followLive ? nowSeconds(state) - FOLLOW_LAG_S : state.viewEnd;
  const vStart = vEnd - span;
  const vx0 = Math.max(0, Math.min(w - 4, toX(vStart)));
  const vx1 = Math.max(vx0 + 4, Math.min(w, toX(vEnd)));

  ctx.fillStyle = theme.selectionFill;
  ctx.fillRect(vx0, 0, vx1 - vx0, h);
  ctx.strokeStyle = theme.selectionStroke;
  ctx.lineWidth = Math.max(1, dpr);
  ctx.strokeRect(vx0 + 0.5, 0.5, Math.max(2, vx1 - vx0 - 1), h - 1);

  if (rangeEl) {
    rangeEl.textContent = `${Math.max(0, vStart).toFixed(2)} s — ${Math.max(0, vEnd).toFixed(2)} s`;
  }
}

export function attachOverviewScrubber(
  state: TimelineState,
  onScrub: (centerTime: number) => void,
): void {
  const track = document.getElementById("overview-canvas") as HTMLCanvasElement | null;
  if (!track) return;
  let scrubbing = false;
  const scrubTo = (clientX: number) => {
    const rect = track.getBoundingClientRect();
    if (rect.width <= 0) return;
    const frac = Math.max(0, Math.min(1, (clientX - rect.left) / rect.width));
    const now = Math.max(nowSeconds(state), state.viewEnd, 1);
    const span = spanSeconds(state.division);
    const start = Math.min(state.historyStart, Math.max(0, state.viewEnd - span));
    const center = start + frac * (now - start);
    onScrub(center);
  };
  track.addEventListener("mousedown", (e) => {
    scrubbing = true;
    scrubTo(e.clientX);
  });
  window.addEventListener("mousemove", (e) => {
    if (scrubbing) scrubTo(e.clientX);
  });
  window.addEventListener("mouseup", () => {
    scrubbing = false;
  });
}
