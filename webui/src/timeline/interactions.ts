import type { SeriesInfo } from "../client";
import {
  FOLLOW_LAG_S,
  WHEEL_STEP,
  zoomDivisionAround,
  spanSeconds,
  pxToTime,
  panYRange,
} from "./math";
import {
  getGroupYScale,
  getTrackOverRect,
  nowSeconds,
  type Pin,
  type TimelineState,
  type TrackRow,
} from "./state";
import type { ContextMenuRequest } from "./contextMenu";

export interface InteractionCallbacks {
  onSelectTrack: (trackName: string) => void;
  onStopFollowing: () => void;
  onRefresh: () => void;
  onRedrawOverlays: () => void;
  onHoverImmediate: () => void;
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
        button: number;
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
    | { mode: "pin"; id: number }
    | null = null;
  let wheelTravel = 0;
  let currentCursor = "default";
  const setCursor = (next: string) => {
    if (currentCursor !== next) {
      currentCursor = next;
      over.style.cursor = next;
    }
  };

  const isNearCursor = (clientX: number, rectLeft: number, cursorVal: number | null): boolean => {
    if (cursorVal === null) return false;
    const px = plot.valToPos(cursorVal, "x", false);
    return Math.abs(clientX - rectLeft - px) <= 6;
  };

  const findNearPin = (clientX: number, rectLeft: number): Pin | undefined => {
    for (let i = state.pins.length - 1; i >= 0; i--) {
      if (isNearCursor(clientX, rectLeft, state.pins[i].t)) {
        return state.pins[i];
      }
    }
    return undefined;
  };

  over.addEventListener(
    "wheel",
    (event) => {
      event.preventDefault();
      const rect = over.getBoundingClientRect();

      // Shift + Wheel: Zoom Y axis of hovered track
      if (event.shiftKey) {
        const rawDelta =
          Math.abs(event.deltaX) > Math.abs(event.deltaY) ? event.deltaX : event.deltaY;
        if (rawDelta === 0) return;
        const yCfg = getGroupYScale(state, groupName);
        const curMin = yCfg.auto ? (plot.scales.y.min ?? -1) : yCfg.min;
        const curMax = yCfg.auto ? (plot.scales.y.max ?? 1) : yCfg.max;
        const span = curMax - curMin || 1;
        const fracY =
          1 - Math.max(0, Math.min(1, (event.clientY - rect.top) / Math.max(1, rect.height)));
        const pivotY = curMin + span * fracY;
        const factor = rawDelta > 0 ? 1.2 : 1 / 1.2;
        const newSpan = span * factor;
        yCfg.auto = false;
        yCfg.min = pivotY - newSpan * fracY;
        yCfg.max = pivotY + newSpan * (1 - fracY);
        callbacks.onApplyTrackY(track);
        return;
      }

      // Ctrl/Meta + Wheel: Zoom time centered at mouse cursor
      if (event.ctrlKey || event.metaKey) {
        wheelTravel += event.deltaY;
        if (Math.abs(wheelTravel) < WHEEL_STEP) return;
        const steps = Math.sign(wheelTravel);
        wheelTravel = 0;
        const fraction = (event.clientX - rect.left) / Math.max(1, rect.width);
        const currentEnd = state.followLive ? nowSeconds(state) - FOLLOW_LAG_S : state.viewEnd;
        const zoomed = zoomDivisionAround(state.division, currentEnd, steps, fraction);
        callbacks.onStopFollowing();
        state.division = zoomed.division;
        state.viewEnd = zoomed.viewEnd;
        callbacks.onRefresh();
        return;
      }

      // Default Wheel: Horizontal scroll in time
      const rawDelta =
        Math.abs(event.deltaX) > Math.abs(event.deltaY) ? event.deltaX : event.deltaY;
      if (rawDelta === 0) return;
      const span = spanSeconds(state.division);
      const step = (rawDelta / 400) * span;
      callbacks.onStopFollowing();
      state.viewEnd = Math.min(nowSeconds(state), state.viewEnd + step);
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
          button: 1,
          x: event.clientX,
          y: event.clientY,
          end: state.followLive ? nowSeconds(state) - FOLLOW_LAG_S : state.viewEnd,
          yMin,
          yMax,
          panX: false,
          panY: false,
        };
        setCursor("grabbing");
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
        setCursor("ns-resize");
        return;
      }

      if (isNearCursor(event.clientX, rect.left, state.cursorB)) {
        drag = { mode: "cursor-b" };
        return;
      }
      if (isNearCursor(event.clientX, rect.left, state.cursorA)) {
        drag = { mode: "cursor-a" };
        return;
      }
      if (isNearCursor(event.clientX, rect.left, state.sampleCursor)) {
        drag = { mode: "cursor-s" };
        return;
      }
      const hitPin = findNearPin(event.clientX, rect.left);
      if (hitPin) {
        drag = { mode: "pin", id: hitPin.id };
        return;
      }

      drag = {
        mode: "pan-xy",
        button: 0,
        x: event.clientX,
        y: event.clientY,
        end: state.followLive ? nowSeconds(state) - FOLLOW_LAG_S : state.viewEnd,
        yMin,
        yMax,
        panX: false,
        panY: false,
      };
      setCursor("grabbing");
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
      const rect = getTrackOverRect(track);
      const relX = Math.max(0, Math.min(rect.width, event.clientX - rect.left));
      state.hoverRelX = relX;
      state.hoverTrack = groupName;
      state.hoverTime = plot.posToVal(relX, "x");
      callbacks.onUpdateReadouts();

      if (!drag) {
        if (
          isNearCursor(event.clientX, rect.left, state.cursorA) ||
          isNearCursor(event.clientX, rect.left, state.cursorB) ||
          isNearCursor(event.clientX, rect.left, state.sampleCursor) ||
          findNearPin(event.clientX, rect.left) !== undefined
        ) {
          setCursor("ew-resize");
        } else if (event.ctrlKey || event.metaKey) {
          setCursor("crosshair");
        } else {
          setCursor("default");
        }
        callbacks.onHoverImmediate();
      }
    },
    { signal },
  );

  over.addEventListener(
    "mouseleave",
    () => {
      if (!drag) {
        state.hoverRelX = null;
        state.hoverTrack = null;
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
      const nearPin = findNearPin(event.clientX, rect.left);
      callbacks.onContextMenu({
        clientX: event.clientX,
        clientY: event.clientY,
        trackName: groupName,
        timeAtCursor,
        pinId: nearPin?.id,
        signal: signalMatch,
      });
    },
    { signal },
  );

  window.addEventListener(
    "mousemove",
    (event) => {
      if (!drag) return;
      const rect = getTrackOverRect(track);
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
      if (drag.mode === "pin") {
        const pinId = drag.id;
        const pin = state.pins.find((p) => p.id === pinId);
        if (pin) {
          pin.t = plot.posToVal(relX, "x");
          callbacks.onUpdateMeasure();
          callbacks.onRedrawOverlays();
        }
        return;
      }
      if (drag.mode === "box") {
        const x1 = relX * (devicePixelRatio || 1) + plot.bbox.left;
        state.boxSelect = { groupName, x0: drag.x0, x1 };
        callbacks.onRedrawOverlays();
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
          callbacks.onRefresh();
        }
      }
    },
    { signal },
  );

  window.addEventListener(
    "mouseup",
    (event) => {
      if (!drag) return;
      setCursor("default");
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
          callbacks.onRedrawOverlays();
        }
        drag = null;
        return;
      }
      if (drag.mode === "pan-xy") {
        if (drag.panX) {
          callbacks.onRefresh();
        } else if (
          drag.button === 0 &&
          !drag.panY &&
          Math.abs(event.clientX - drag.x) < 4 &&
          Math.abs(event.clientY - drag.y) < 4
        ) {
          const rect = over.getBoundingClientRect();
          const relX = Math.max(0, Math.min(rect.width, event.clientX - rect.left));
          const t = plot.posToVal(relX, "x");
          callbacks.onPlaceSampleCursor(t);
        }
      }
      drag = null;
    },
    { signal },
  );

  return ac;
}
