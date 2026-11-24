import type { Client, TimelineEvent } from "../client";
import type { ThemeManager, ThemePreference } from "../theme";
import { eventKindInfo } from "./cursors";
import { KEYBOARD_HELP, MOUSE_HELP } from "./keymap";
import { FOLLOW_LAG_S, formatSeconds, spanSeconds, stepDivision } from "./math";
import {
  LEGEND_WIDTH_KEY,
  nowSeconds,
  saveExprHistory,
  type TimelineState,
} from "./state";

export interface RateReply {
  rateHz: number;
  maxHz: number;
}

export interface ToolbarCallbacks {
  loadSeries: () => Promise<void>;
  refresh: () => Promise<void>;
  rebuild: () => void;
  fitAllHistory: () => void;
  resizeAll: () => void;
  clearCursors: () => void;
  zoomToRange: (a: number, b: number) => void;
}

const byId = <T extends HTMLElement>(id: string) => document.getElementById(id) as T | null;

export function syncDivisionControls(state: TimelineState): void {
  const select = byId<HTMLSelectElement>("division");
  const label = byId("span-label");
  if (select) select.value = String(state.division);
  if (label) label.textContent = formatSeconds(spanSeconds(state.division));
}

export function syncFollowButton(state: TimelineState): void {
  const button = byId<HTMLButtonElement>("follow");
  if (!button) return;
  if (state.followLive) {
    button.textContent = "Following";
  } else {
    const lag = Math.max(0, nowSeconds(state) - state.viewEnd);
    button.textContent = lag > 0.05 ? `Paused (-${formatSeconds(lag)})` : "Paused";
  }
  button.setAttribute("aria-pressed", String(state.followLive));
}

export function stopFollowing(state: TimelineState): void {
  if (state.followLive) {
    state.viewEnd = nowSeconds(state) - FOLLOW_LAG_S;
  }
  state.followLive = false;
  syncFollowButton(state);
}

export function showRate(rate: RateReply): void {
  const select = byId<HTMLSelectElement>("rate");
  if (!select) return;
  const value = String(rate.rateHz);
  if (![...select.options].some((option) => option.value === value)) {
    select.append(new Option(`${value} Hz`, value));
  }
  select.value = value;
  select.classList.toggle("off", rate.rateHz === 0);
}

export function syncRecordButton(state: TimelineState, active: boolean): void {
  state.recording = active;
  const button = byId<HTMLButtonElement>("record");
  if (!button) return;
  button.textContent = active ? "● Stop Rec" : "Record";
  button.classList.toggle("active", active);
}

export function setConnectionStatus(
  text: string,
  status: "connected" | "disconnected" | "reconnecting",
): void {
  const conn = byId("connection");
  if (!conn) return;
  const dot = document.createElement("span");
  dot.className = `status-dot ${
    status === "connected" ? "connected" : status === "disconnected" ? "disconnected" : ""
  }`;
  conn.replaceChildren(dot, document.createTextNode(text));
}

export function updateSuggestions(state: TimelineState): void {
  const exprList = byId<HTMLDataListElement>("expr-suggestions");
  const mode = byId<HTMLSelectElement>("mode")?.value;
  if (exprList) {
    const items =
      mode === "rtt" ? state.rttSignals.map((s) => s.name) : state.exprHistory;
    exprList.replaceChildren(...items.map((val) => new Option(val, val)));
  }

  const groupList = byId<HTMLDataListElement>("group-suggestions");
  if (groupList) {
    const groupNames = [
      ...new Set(state.series.map((s) => s.group).filter((g): g is string => Boolean(g))),
    ];
    groupList.replaceChildren(...groupNames.map((g) => new Option(g, g)));
  }
}

export function updateEventsUi(
  state: TimelineState,
  onJumpToEvent: (ev: TimelineEvent) => void,
): void {
  const notable = state.allEvents.filter(
    (e) => e.kind === 1 || e.kind === 3 || e.kind === 4 || e.kind === 6,
  );
  const btn = byId<HTMLButtonElement>("events-btn");
  if (btn) btn.textContent = `Events: ${notable.length}`;

  const list = byId("events-list");
  if (!list) return;
  if (notable.length === 0) {
    list.innerHTML = `<div class="drawer-empty">No target events recorded yet.</div>`;
    return;
  }
  list.replaceChildren(
    ...notable
      .slice()
      .reverse()
      .slice(0, 60)
      .map((ev) => {
        const info = eventKindInfo(state, ev.kind);
        const row = document.createElement("div");
        row.className = "event-row";
        row.title = "Click to jump Timeline to this event";
        const tag = document.createElement("span");
        tag.className = `event-tag ${info.cls}`;
        tag.textContent = info.label;
        const time = document.createElement("span");
        time.textContent = `t = ${ev.t.toFixed(4)} s`;
        row.append(tag, time);
        row.onclick = () => onJumpToEvent(ev);
        return row;
      }),
  );
}

export function exportVisibleCsv(client: Client, state: TimelineState): Promise<void> {
  const end = state.followLive ? nowSeconds(state) : state.viewEnd;
  const span = spanSeconds(state.division);
  const measureEl = byId("measure");
  return client
    .call<{ path: string; rows: number }>("record.export", {
      format: "csv",
      seriesIds: state.series.map((item) => item.seriesId),
      t0: Math.round((end - span) * 1e9),
      t1: Math.round(end * 1e9),
    })
    .then((result) => {
      if (measureEl) measureEl.textContent = `Wrote ${result.rows} rows to ${result.path}`;
    })
    .catch((error) => {
      if (measureEl) measureEl.textContent = String(error);
    });
}

export function exportPngSnapshot(state: TimelineState): void {
  const measureEl = byId("measure");
  const activeTracks = state.tracks.filter(
    (g) => !(state.groupYScales.get(g.name)?.collapsed ?? false),
  );
  if (activeTracks.length === 0) {
    if (measureEl) measureEl.textContent = "Nothing to export: plot at least one signal first.";
    return;
  }
  const dpr = devicePixelRatio || 1;
  const pad = Math.round(16 * dpr);
  const headerH = Math.round(36 * dpr);
  const trackTitleH = Math.round(22 * dpr);
  const width = Math.max(...activeTracks.map((g) => g.plot.ctx.canvas.width)) + pad * 2;
  const plotsH = activeTracks.reduce(
    (sum, g) => sum + g.plot.ctx.canvas.height + trackTitleH + Math.round(8 * dpr),
    0,
  );
  const height = headerH + plotsH + pad * 2;

  const out = document.createElement("canvas");
  out.width = width;
  out.height = height;
  const ctx = out.getContext("2d");
  if (!ctx) return;

  ctx.fillStyle = state.canvasTheme.bg;
  ctx.fillRect(0, 0, width, height);

  ctx.fillStyle = state.canvasTheme.axisStroke;
  ctx.font = `bold ${Math.round(13 * dpr)}px system-ui, sans-serif`;
  const statusText = byId("connection")?.textContent ?? "Haltd Timeline";
  const measureText = measureEl?.textContent ?? "";
  ctx.fillText(`Haltd Timeline — ${statusText}   ${measureText}`, pad, pad + Math.round(16 * dpr));

  let y = pad + headerH;
  for (const g of activeTracks) {
    ctx.fillStyle = state.canvasTheme.axisStroke;
    ctx.font = `600 ${Math.round(11 * dpr)}px ui-monospace, monospace`;
    const memberSummary = g.members.map((m) => m.name).join(", ");
    ctx.fillText(`${g.name}: ${memberSummary}`, pad, y + Math.round(14 * dpr));
    y += trackTitleH;
    ctx.drawImage(g.plot.ctx.canvas, pad, y);
    y += g.plot.ctx.canvas.height + Math.round(8 * dpr);
  }

  const link = document.createElement("a");
  link.download = `haltd-timeline-${Date.now()}.png`;
  link.href = out.toDataURL("image/png");
  link.click();
  if (measureEl) measureEl.textContent = `Saved PNG snapshot (${link.download})`;
}

export function renderHelpModal(): void {
  const kbdGrid = byId("help-keyboard-grid");
  const mouseGrid = byId("help-mouse-grid");
  if (kbdGrid) {
    kbdGrid.replaceChildren(
      ...KEYBOARD_HELP.flatMap((item) => {
        const keyCell = document.createElement("div");
        const kbd = document.createElement("kbd");
        kbd.textContent = item.keys;
        keyCell.append(kbd);
        const descCell = document.createElement("div");
        descCell.textContent = item.description;
        return [keyCell, descCell];
      }),
    );
  }
  if (mouseGrid) {
    mouseGrid.replaceChildren(
      ...MOUSE_HELP.flatMap((item) => {
        const keyCell = document.createElement("div");
        const kbd = document.createElement("kbd");
        kbd.textContent = item.keys;
        keyCell.append(kbd);
        const descCell = document.createElement("div");
        descCell.textContent = item.description;
        return [keyCell, descCell];
      }),
    );
  }
}

export function wireToolbar(
  client: Client,
  state: TimelineState,
  themeManager: ThemeManager,
  callbacks: ToolbarCallbacks,
): void {
  renderHelpModal();

  // Theme selector
  const themeSelect = byId<HTMLSelectElement>("theme-select");
  if (themeSelect) {
    themeSelect.value = themeManager.getPreference();
    themeSelect.addEventListener("change", () => {
      themeManager.setPreference(themeSelect.value as ThemePreference);
    });
  }

  // Add signal form
  byId<HTMLSelectElement>("mode")?.addEventListener("change", () => updateSuggestions(state));
  byId<HTMLFormElement>("add")?.addEventListener("submit", async (event) => {
    event.preventDefault();
    const input = byId<HTMLInputElement>("expr");
    if (!input) return;
    const expr = input.value.trim();
    if (!expr) return;
    const mode = byId<HTMLSelectElement>("mode")?.value ?? "auto";
    const group = byId<HTMLInputElement>("group")?.value.trim() ?? "";
    const measureEl = byId("measure");
    try {
      if (mode === "rtt") {
        await client.call("timeline.add", {
          rttSignal: expr,
          ...(group ? { group } : {}),
        });
      } else {
        await client.call("timeline.add", {
          expr,
          mode,
          ...(group ? { group } : {}),
        });
        state.exprHistory = [
          expr,
          ...state.exprHistory.filter((item) => item !== expr),
        ].slice(0, 24);
        saveExprHistory(state.exprHistory);
        updateSuggestions(state);
      }
      input.value = "";
      await callbacks.loadSeries();
    } catch (error) {
      if (measureEl) measureEl.textContent = String(error);
    }
  });

  // Rate & Timebase
  byId<HTMLSelectElement>("rate")?.addEventListener("change", async (event) => {
    const rateHz = Number((event.target as HTMLSelectElement).value);
    const measureEl = byId("measure");
    try {
      showRate(await client.call<RateReply>("timeline.setRate", { rateHz }));
      await callbacks.loadSeries();
    } catch (error) {
      if (measureEl) measureEl.textContent = String(error);
      showRate(await client.call<RateReply>("timeline.rate"));
    }
  });

  byId<HTMLSelectElement>("division")?.addEventListener("change", (event) => {
    state.division = Number((event.target as HTMLSelectElement).value);
    syncDivisionControls(state);
    void callbacks.refresh();
  });

  byId("zoom-in")?.addEventListener("click", () => {
    state.division = stepDivision(state.division, -1);
    syncDivisionControls(state);
    void callbacks.refresh();
  });

  byId("zoom-out")?.addEventListener("click", () => {
    state.division = stepDivision(state.division, 1);
    syncDivisionControls(state);
    void callbacks.refresh();
  });

  // Navigation & Target controls
  byId("pan-left")?.addEventListener("click", () => {
    stopFollowing(state);
    state.viewEnd -= state.division;
    syncFollowButton(state);
    void callbacks.refresh();
  });
  byId("pan-right")?.addEventListener("click", () => {
    stopFollowing(state);
    state.viewEnd = Math.min(nowSeconds(state), state.viewEnd + state.division);
    syncFollowButton(state);
    void callbacks.refresh();
  });
  byId("follow")?.addEventListener("click", () => {
    state.followLive = !state.followLive;
    if (state.followLive) {
      state.viewEnd = nowSeconds(state) - FOLLOW_LAG_S;
    }
    syncFollowButton(state);
    void callbacks.refresh();
  });
  byId("fit-btn")?.addEventListener("click", () => callbacks.fitAllHistory());

  byId("target-halt")?.addEventListener("click", () => {
    void client.call("target.halt").catch((error) => {
      const m = byId("measure");
      if (m) m.textContent = String(error);
    });
  });
  byId("target-go")?.addEventListener("click", () => {
    void client.call("target.go").catch((error) => {
      const m = byId("measure");
      if (m) m.textContent = String(error);
    });
  });
  byId("target-reset")?.addEventListener("click", () => {
    void client.call("target.reset").catch((error) => {
      const m = byId("measure");
      if (m) m.textContent = String(error);
    });
  });

  // Layout, Bands, Record, Export, Snapshot
  byId("layout-mode")?.addEventListener("click", () => {
    state.splitLanes = !state.splitLanes;
    const btn = byId<HTMLButtonElement>("layout-mode");
    if (btn) {
      btn.textContent = state.splitLanes ? "Split" : "Grouped";
      btn.setAttribute("aria-pressed", String(state.splitLanes));
    }
    callbacks.rebuild();
  });
  byId("toggle-band")?.addEventListener("click", () => {
    state.showBands = !state.showBands;
    byId("toggle-band")?.setAttribute("aria-pressed", String(state.showBands));
    callbacks.rebuild();
  });
  byId("record")?.addEventListener("click", async () => {
    const measureEl = byId("measure");
    try {
      if (state.recording) {
        const result = await client.call<{ path: string; samples: number }>("record.stop");
        if (measureEl) measureEl.textContent = `Saved ${result.samples} samples to ${result.path}`;
        syncRecordButton(state, false);
      } else {
        await client.call("record.start");
        syncRecordButton(state, true);
      }
    } catch (error) {
      if (measureEl) measureEl.textContent = String(error);
    }
  });
  byId("export")?.addEventListener("click", () => void exportVisibleCsv(client, state));
  byId("snapshot")?.addEventListener("click", () => exportPngSnapshot(state));

  // Filter & batch visibility
  byId<HTMLInputElement>("series-filter")?.addEventListener("input", (event) => {
    state.seriesFilter = (event.target as HTMLInputElement).value;
    callbacks.rebuild();
  });
  byId("btn-show-all")?.addEventListener("click", () => {
    state.soloSeriesId = null;
    state.hidden.clear();
    callbacks.rebuild();
  });
  byId("btn-hide-all")?.addEventListener("click", () => {
    state.soloSeriesId = null;
    state.series.forEach((s) => state.hidden.add(s.seriesId));
    callbacks.rebuild();
  });
  byId("btn-clear-all")?.addEventListener("click", async () => {
    state.soloSeriesId = null;
    state.hidden.clear();
    await Promise.all(
      state.series.map((s) => client.call("timeline.remove", { seriesId: s.seriesId })),
    );
    await callbacks.loadSeries();
  });

  // Cursor panel buttons & bottom drawer tabs
  byId("cursor-clear")?.addEventListener("click", () => callbacks.clearCursors());
  byId("cursor-zoom")?.addEventListener("click", () => {
    if (state.cursorA !== null && state.cursorB !== null) {
      callbacks.zoomToRange(state.cursorA, state.cursorB);
    }
  });

  const drawer = byId("bottom-drawer");
  const rttTab = byId<HTMLButtonElement>("tab-rtt");
  const curTab = byId<HTMLButtonElement>("tab-cursors");
  const rttPane = byId("drawer-rtt");
  const curPane = byId("drawer-cursors");
  const closeDrawerBtn = byId("drawer-close");

  const openDrawerTab = (tab: "rtt" | "cursors" | null) => {
    if (!drawer || !rttTab || !curTab || !rttPane || !curPane) return;
    if (tab === null) {
      drawer.hidden = true;
      rttTab.setAttribute("aria-pressed", "false");
      curTab.setAttribute("aria-pressed", "false");
    } else {
      const same = !drawer.hidden && ((tab === "rtt" && !rttPane.hidden) || (tab === "cursors" && !curPane.hidden));
      if (same) {
        drawer.hidden = true;
        rttTab.setAttribute("aria-pressed", "false");
        curTab.setAttribute("aria-pressed", "false");
      } else {
        drawer.hidden = false;
        rttPane.hidden = tab !== "rtt";
        curPane.hidden = tab !== "cursors";
        rttTab.setAttribute("aria-pressed", String(tab === "rtt"));
        curTab.setAttribute("aria-pressed", String(tab === "cursors"));
      }
    }
    callbacks.resizeAll();
  };

  rttTab?.addEventListener("click", () => openDrawerTab("rtt"));
  curTab?.addEventListener("click", () => openDrawerTab("cursors"));
  closeDrawerBtn?.addEventListener("click", () => openDrawerTab(null));

  // Legend column width resizer
  const legendResizer = byId("legend-resizer");
  const root = document.documentElement;
  try {
    const saved = Number(localStorage.getItem(LEGEND_WIDTH_KEY));
    if (saved >= 200 && saved <= 440) {
      root.style.setProperty("--legend-width", `${saved}px`);
    }
  } catch {
    // Ignore storage errors
  }
  if (legendResizer) {
    let resizing = false;
    let startX = 0;
    let startW = 265;
    legendResizer.addEventListener("mousedown", (e) => {
      resizing = true;
      startX = e.clientX;
      const cur = parseFloat(getComputedStyle(root).getPropertyValue("--legend-width")) || 265;
      startW = cur;
      legendResizer.classList.add("dragging");
      e.preventDefault();
    });
    window.addEventListener("mousemove", (e) => {
      if (!resizing) return;
      const w = Math.max(200, Math.min(440, Math.round(startW + (e.clientX - startX))));
      root.style.setProperty("--legend-width", `${w}px`);
      callbacks.resizeAll();
    });
    window.addEventListener("mouseup", () => {
      if (!resizing) return;
      resizing = false;
      legendResizer.classList.remove("dragging");
      const cur = parseFloat(getComputedStyle(root).getPropertyValue("--legend-width")) || 265;
      try {
        localStorage.setItem(LEGEND_WIDTH_KEY, String(Math.round(cur)));
      } catch {
        // Ignore storage errors
      }
    });
  }

  // Events popover & Help modal
  byId("events-btn")?.addEventListener("click", () => {
    const pop = byId("events-popover");
    if (pop) pop.hidden = !pop.hidden;
  });
  byId("events-close")?.addEventListener("click", () => {
    const pop = byId("events-popover");
    if (pop) pop.hidden = true;
  });
  byId("help-btn")?.addEventListener("click", () => {
    const modal = byId("help-modal");
    if (modal) modal.hidden = false;
  });
  byId("help-close")?.addEventListener("click", () => {
    const modal = byId("help-modal");
    if (modal) modal.hidden = true;
  });
  byId("help-modal")?.addEventListener("click", (e) => {
    const modal = byId("help-modal");
    if (modal && e.target === modal) modal.hidden = true;
  });
}
