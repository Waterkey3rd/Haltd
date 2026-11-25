import type { SeriesInfo } from "../client";
import { effectiveGroupOf, type TimelineState } from "./state";

export interface ContextMenuActions {
  zoomToFit: () => void;
  autoScaleTrackY: (trackName: string) => void;
  placeCursorA: (time: number) => void;
  placeCursorB: (time: number) => void;
  placeSampleCursor: (time: number) => void;
  addPin: (time: number) => void;
  removePin: (id: number) => void;
  clearPins: () => void;
  clearCursors: () => void;
  changeSignalColor: (item: SeriesInfo) => void;
  moveSignalToTrack: (item: SeriesInfo, targetTrack: string | null) => void;
  toggleHideSignal: (item: SeriesInfo) => void;
  toggleSoloSignal: (item: SeriesInfo) => void;
  removeSignal: (item: SeriesInfo) => void;
  exportVisibleCsv: () => void;
}

export interface ContextMenuRequest {
  clientX: number;
  clientY: number;
  trackName: string;
  timeAtCursor: number | null;
  pinId?: number;
  signal?: SeriesInfo;
}

let activeMenu: HTMLElement | null = null;

export function hideContextMenu(): void {
  if (activeMenu) {
    activeMenu.remove();
    activeMenu = null;
  }
}

export function showContextMenu(
  state: TimelineState,
  req: ContextMenuRequest,
  actions: ContextMenuActions,
): void {
  hideContextMenu();

  const menu = document.createElement("div");
  menu.className = "context-menu";
  menu.setAttribute("role", "menu");

  const addItem = (
    label: string,
    shortcut: string,
    onClick: () => void,
    disabled = false,
  ): HTMLElement => {
    const item = document.createElement("button");
    item.type = "button";
    item.className = "context-item";
    item.disabled = disabled;
    const lbl = document.createElement("span");
    lbl.textContent = label;
    const kbd = document.createElement("span");
    kbd.className = "context-shortcut";
    kbd.textContent = shortcut;
    item.append(lbl, kbd);
    item.onclick = () => {
      hideContextMenu();
      onClick();
    };
    menu.append(item);
    return item;
  };

  const addDivider = () => {
    const sep = document.createElement("div");
    sep.className = "context-sep";
    menu.append(sep);
  };

  addItem("Zoom to Fit", "Ctrl+0", () => actions.zoomToFit());
  addItem("Auto-Scale Y (This Track)", "Y", () => actions.autoScaleTrackY(req.trackName));

  addDivider();

  addItem(
    "Place Cursor A Here",
    "A",
    () => {
      if (req.timeAtCursor !== null) actions.placeCursorA(req.timeAtCursor);
    },
    req.timeAtCursor === null,
  );
  addItem(
    "Place Cursor B Here",
    "B",
    () => {
      if (req.timeAtCursor !== null) actions.placeCursorB(req.timeAtCursor);
    },
    req.timeAtCursor === null,
  );
  addItem(
    "Pin Sample Cursor Here",
    "Click",
    () => {
      if (req.timeAtCursor !== null) actions.placeSampleCursor(req.timeAtCursor);
    },
    req.timeAtCursor === null,
  );
  addItem(
    "Add Pin Here",
    "",
    () => {
      if (req.timeAtCursor !== null) actions.addPin(req.timeAtCursor);
    },
    req.timeAtCursor === null,
  );
  if (req.pinId !== undefined) {
    const targetPin = state.pins.find((p) => p.id === req.pinId);
    if (targetPin) {
      addItem(`Remove Pin ${targetPin.label}`, "", () => actions.removePin(targetPin.id));
    }
  }
  addItem(
    "Clear All Pins",
    "",
    () => actions.clearPins(),
    state.pins.length === 0 && state.sampleCursor === null,
  );
  addItem(
    "Clear Cursors",
    "Esc",
    () => actions.clearCursors(),
    state.cursorA === null && state.cursorB === null && state.sampleCursor === null,
  );

  const trackSignals = state.series.filter((s) => effectiveGroupOf(state, s) === req.trackName);
  const targetSignal =
    req.signal ??
    trackSignals.find((s) => s.seriesId === state.selectedSeriesId) ??
    trackSignals[0];

  if (targetSignal) {
    addDivider();

    addItem(`Change Color (${targetSignal.name})…`, "", () =>
      actions.changeSignalColor(targetSignal),
    );

    // Move to Track submenu
    const moveRow = document.createElement("div");
    moveRow.className = "context-item context-submenu-trigger";
    const moveLbl = document.createElement("span");
    moveLbl.textContent = `Move ${targetSignal.name} to Track`;
    const arrow = document.createElement("span");
    arrow.className = "context-shortcut";
    arrow.textContent = "▸";
    moveRow.append(moveLbl, arrow);

    const sub = document.createElement("div");
    sub.className = "context-submenu";
    const existingTracks = [...new Set(state.series.map((s) => effectiveGroupOf(state, s)))];
    const currentTrack = effectiveGroupOf(state, targetSignal);

    const newTrackBtn = document.createElement("button");
    newTrackBtn.type = "button";
    newTrackBtn.className = "context-item";
    newTrackBtn.textContent = "+ New Track";
    newTrackBtn.onclick = () => {
      hideContextMenu();
      actions.moveSignalToTrack(targetSignal, null);
    };
    sub.append(newTrackBtn);

    if (existingTracks.length > 0) {
      const subSep = document.createElement("div");
      subSep.className = "context-sep";
      sub.append(subSep);
    }

    for (const track of existingTracks) {
      const tBtn = document.createElement("button");
      tBtn.type = "button";
      tBtn.className = "context-item";
      tBtn.disabled = track === currentTrack;
      tBtn.textContent = track === currentTrack ? `✓ ${track}` : track;
      tBtn.onclick = () => {
        hideContextMenu();
        actions.moveSignalToTrack(targetSignal, track);
      };
      sub.append(tBtn);
    }

    moveRow.append(sub);
    menu.append(moveRow);

    addItem(
      state.hidden.has(targetSignal.seriesId) ? "Show Signal" : "Hide Signal",
      "H",
      () => actions.toggleHideSignal(targetSignal),
    );
    addItem(
      state.soloSeriesId === targetSignal.seriesId ? "Unsolo Signal" : "Solo Signal",
      "S",
      () => actions.toggleSoloSignal(targetSignal),
    );
    addItem(`Remove ${targetSignal.name}`, "Del", () => actions.removeSignal(targetSignal));
  }

  addDivider();
  const csvLabel =
    state.cursorA !== null && state.cursorB !== null
      ? "Export Cursor A..B to CSV…"
      : "Export CSV…";
  addItem(csvLabel, "", () => actions.exportVisibleCsv());

  document.body.append(menu);
  activeMenu = menu;

  // Position within viewport bounds
  const rect = menu.getBoundingClientRect();
  const left = Math.min(req.clientX, Math.max(4, window.innerWidth - rect.width - 6));
  const top = Math.min(req.clientY, Math.max(4, window.innerHeight - rect.height - 6));
  menu.style.left = `${left}px`;
  menu.style.top = `${top}px`;
}

export function initContextMenuDismiss(): void {
  window.addEventListener("mousedown", (event) => {
    if (activeMenu && !activeMenu.contains(event.target as Node)) {
      hideContextMenu();
    }
  });
  window.addEventListener("resize", hideContextMenu);
  window.addEventListener("blur", hideContextMenu);
}
