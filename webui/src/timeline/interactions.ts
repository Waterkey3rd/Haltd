import type { SeriesInfo } from "../client";
import { WHEEL_STEP, zoomDivisionAround, spanSeconds, pxToTime, panYRange } from "./math";
import { getGroupYScale, nowSeconds, type TimelineState, type TrackRow } from "./state";
import type { ContextMenuRequest } from "./contextMenu";

export interface InteractionCallbacks {
  onSelectTrack: (trackName: string) => void;
  onStopFollowing: () => void;
  onRefresh: () => void;
  onRedrawOverlays: () => void;
  onUpdateReadouts: () => void;
  onUpdateMeasure: () => void;
  onApplyTrackY: (track: TrackRow) => void;
  onZoomToRange: (t0: number, t1: number) => void;
  onPlaceSampleCursor: (time: number) => void;
  onContextMenu: (req: ContextMenuRequest) => void;
}

export function attachTrackInteractions(
  state: TimelineState,
  track: TrackRow,
  callbacks: InteractionCallbacks,
): AbortController {
  const ac = new AbortController();
  const { signal } = ac;
  const { plot, name: groupName } = track;
  const over = plot.over;

  let drag:
    | {
        mode: "pan-xy";
        x: number;
        y: number;
        end: number;
        yMin: number;
        yMax: number;
        panX: boolean;
        panY: boolean;
      }
    | { mode: "pan-y"; y: number; yMin: number; yMax: number }
    | { mode: "box"; x0: number }
    | { mode: "cursor-s" }
    | { mode: "cursor-a" }
    | { mode: "cursor-b" }
    | null = null;
  let wheelTravel = 0;

  const isNearCursor = (clientX: number, cursorVal: number | null): boolean => {
    if (cursorVal === null) return false;
    const rect = over.getBoundingClientRect();
    const px = plot.valToPos(cursorVal, "x", false);
    return Math.abs(clientX - rect.left - px) <= 6;
  };

  over.addEventListener(
    "wheel",
    (event) => {
      event.preventDefault();
      const rect = over.getBoundingClientRect();

      // Ctrl + Wheel: Zoom Y axis of hovered track
      if (event.ctrlKey || event.metaKey) {
        const yCfg = getGroupYScale(state, groupName);
        const curMin = yCfg.auto ? (plot.scales.y.min ?? -1) : yCfg.min;
        const curMax = yCfg.auto ? (plot.scales.y.max ?? 1) : yCfg.max;
        const span = curMax - curMin || 1;
        const fracY =
          1 - Math.max(0, Math.min(1, (event.clientY - rect.top) / Math.max(1, rect.height)));
        const pivotY = curMin + span * fracY;
        const factor = event.deltaY > 0 ? 1.2 : 1 / 1.2;
        const newSpan = span * factor;
        yCfg.auto = false;
        yCfg.min = pivotY - newSpan * fracY;
        yCfg.max = pivotY + newSpan * (1 - fracY);
        callbacks.onApplyTrackY(track);
        return;
      }

      // Shift + Wheel: Horizontal scroll in time
      if (event.shiftKey) {
        const rawDelta =
          Math.abs(event.deltaX) > Math.abs(event.deltaY) ? event.deltaX : event.deltaY;
        if (rawDelta === 0) return;
        const span = spanSeconds(state.division);
        const step = (rawDelta / 400) * span;
        callbacks.onStopFollowing();
        state.viewEnd = Math.min(nowSeconds(state), state.viewEnd + step);
        callbacks.onRefresh();
        return;
      }

      // Default Wheel: Zoom time centered at mouse cursor (Ozone style)
      wheelTravel += event.deltaY;
      if (Math.abs(wheelTravel) < WHEEL_STEP) return;
      const steps = Math.sign(wheelTravel);
      wheelTravel = 0;
      const fraction = (event.clientX - rect.left) / Math.max(1, rect.width);
      const currentEnd = state.followLive ? nowSeconds(state) - 0.03 : state.viewEnd;
      const zoomed = zoomDivisionAround(state.division, currentEnd, steps, fraction);
      callbacks.onStopFollowing();
      state.division = zoomed.division;
      state.viewEnd = zoomed.viewEnd;
      callbacks.onRefresh();
    },
    { passive: false, signal },
  );

  over.addEventListener(
    "mousedown",
    (event) => {
      callbacks.onSelectTrack(groupName);
      const yCfg = getGroupYScale(state, groupName);
      const yMin = yCfg.auto ? (plot.scales.y.min ?? -1) : yCfg.min;
      const yMax = yCfg.auto ? (plot.scales.y.max ?? 1) : yCfg.max;

      if (event.button === 1) {
        event.preventDefault();
        drag = {
          mode: "pan-xy",
          x: event.clientX,
          y: event.clientY,
          end: state.followLive ? nowSeconds(state) - 0.03 : state.viewEnd,
          yMin,
          yMax,
          panX: false,
          panY: false,
        };
        over.style.cursor = "grabbing";
        return;
      }
      if (event.button !== 0) return;
      const rect = over.getBoundingClientRect();

      if (event.ctrlKey || event.metaKey) {
        const x0 = (event.clientX - rect.left) * (devicePixelRatio || 1) + plot.bbox.left;
        drag = { mode: "box", x0 };
        state.boxSelect = { groupName, x0, x1: x0 };
        return;
      }

      if (event.shiftKey) {
        drag = { mode: "pan-y", y: event.clientY, yMin, yMax };
        over.style.cursor = "ns-resize";
        return;
      }

      if (isNearCursor(event.clientX, state.cursorB)) {
        drag = { mode: "cursor-b" };
        return;
      }
      if (isNearCursor(event.clientX, state.cursorA)) {
        drag = { mode: "cursor-a" };
        return;
      }
      if (isNearCursor(event.clientX, state.sampleCursor)) {
        drag = { mode: "cursor-s" };
        return;
      }

      drag = {
        mode: "pan-xy",
        x: event.clientX,
        y: event.clientY,
        end: state.followLive ? nowSeconds(state) - 0.03 : state.viewEnd,
        yMin,
        yMax,
        panX: false,
        panY: false,
      };
      over.style.cursor = "grabbing";
    },
    { signal },
  );

  over.addEventListener(
    "auxclick",
    (event) => {
      if (event.button === 1) event.preventDefault();
    },
    { signal },
  );

  over.addEventListener(
    "mousemove",
    (event) => {
      const rect = over.getBoundingClientRect();
      const relX = Math.max(0, Math.min(rect.width, event.clientX - rect.left));
      state.hoverTime = plot.posToVal(relX, "x");
      callbacks.onUpdateReadouts();

      if (!drag) {
        if (
          isNearCursor(event.clientX, state.cursorA) ||
          isNearCursor(event.clientX, state.cursorB) ||
          isNearCursor(event.clientX, state.sampleCursor)
        ) {
          over.style.cursor = "ew-resize";
        } else if (event.ctrlKey || event.metaKey) {
          over.style.cursor = "crosshair";
        } else {
          over.style.cursor = "default";
        }
        callbacks.onRedrawOverlays();
      }
    },
    { signal },
  );

  over.addEventListener(
    "mouseleave",
    () => {
      if (!drag) {
        state.hoverTime = null;
        callbacks.onUpdateReadouts();
        callbacks.onRedrawOverlays();
      }
    },
    { signal },
  );

  over.addEventListener(
    "dblclick",
    (event) => {
      const rect = over.getBoundingClientRect();
      const relX = Math.max(0, Math.min(rect.width, event.clientX - rect.left));
      const t = plot.posToVal(relX, "x");
      callbacks.onPlaceSampleCursor(t);
    },
    { signal },
  );

  over.addEventListener(
    "contextmenu",
    (event) => {
      event.preventDefault();
      const rect = over.getBoundingClientRect();
      const relX = Math.max(0, Math.min(rect.width, event.clientX - rect.left));
      const min = plot.scales.x.min ?? 0;
      const max = plot.scales.x.max ?? 0;
      const timeAtCursor =
        max > min ? pxToTime(relX, rect.width, min, max) : plot.posToVal(relX, "x");
      const signalMatch: SeriesInfo | undefined =
        track.members.find((m) => m.seriesId === state.selectedSeriesId) ?? track.members[0];
      callbacks.onContextMenu({
        clientX: event.clientX,
        clientY: event.clientY,
        trackName: groupName,
        timeAtCursor,
        signal: signalMatch,
      });
    },
    { signal },
  );

  window.addEventListener(
    "mousemove",
    (event) => {
      if (!drag) return;
      const rect = over.getBoundingClientRect();
      const relX = Math.max(0, Math.min(rect.width, event.clientX - rect.left));

      if (drag.mode === "cursor-s") {
        state.sampleCursor = plot.posToVal(relX, "x");
        callbacks.onUpdateReadouts();
        callbacks.onUpdateMeasure();
        callbacks.onRedrawOverlays();
        return;
      }
      if (drag.mode === "cursor-a") {
        state.cursorA = plot.posToVal(relX, "x");
        callbacks.onUpdateMeasure();
        callbacks.onRedrawOverlays();
        return;
      }
      if (drag.mode === "cursor-b") {
        state.cursorB = plot.posToVal(relX, "x");
        callbacks.onUpdateMeasure();
        callbacks.onRedrawOverlays();
        return;
      }
      if (drag.mode === "box") {
        const x1 = relX * (devicePixelRatio || 1) + plot.bbox.left;
        state.boxSelect = { groupName, x0: drag.x0, x1 };
        plot.redraw(false);
        return;
      }
      if (drag.mode === "pan-y") {
        const dy = event.clientY - drag.y;
        if (Math.abs(dy) > 2) {
          const yCfg = getGroupYScale(state, groupName);
          const next = panYRange(drag.yMin, drag.yMax, dy, over.clientHeight || rect.height);
          yCfg.auto = false;
          yCfg.min = next.min;
          yCfg.max = next.max;
          callbacks.onApplyTrackY(track);
        }
        return;
      }
      if (drag.mode === "pan-xy") {
        const dx = event.clientX - drag.x;
        const dy = event.clientY - drag.y;
        if (!drag.panY && Math.abs(dy) > 4) {
          drag.panY = true;
        }
        if (!drag.panX && Math.abs(dx) > (drag.panY ? 12 : 4)) {
          drag.panX = true;
        }
        if (drag.panY) {
          const yCfg = getGroupYScale(state, groupName);
          const next = panYRange(drag.yMin, drag.yMax, dy, over.clientHeight || rect.height);
          yCfg.auto = false;
          yCfg.min = next.min;
          yCfg.max = next.max;
          callbacks.onApplyTrackY(track);
        }
        if (drag.panX) {
          callbacks.onStopFollowing();
          state.viewEnd =
            drag.end - (dx / Math.max(1, over.clientWidth)) * spanSeconds(state.division);
        }
      }
    },
    { signal },
  );

  window.addEventListener(
    "mouseup",
    () => {
      if (!drag) return;
      over.style.cursor = "default";
      if (drag.mode === "box" && state.boxSelect) {
        const dpr = devicePixelRatio || 1;
        const p0 = (Math.min(state.boxSelect.x0, state.boxSelect.x1) - plot.bbox.left) / dpr;
        const p1 = (Math.max(state.boxSelect.x0, state.boxSelect.x1) - plot.bbox.left) / dpr;
        state.boxSelect = null;
        if (p1 - p0 > 8) {
          const t0 = plot.posToVal(p0, "x");
          const t1 = plot.posToVal(p1, "x");
          callbacks.onZoomToRange(t0, t1);
        } else {
          plot.redraw(false);
        }
        drag = null;
        return;
      }
      if (drag.mode === "pan-xy" && drag.panX) {
        callbacks.onRefresh();
      }
      drag = null;
    },
    { signal },
  );

  return ac;
}
