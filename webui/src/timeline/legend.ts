import type { SeriesInfo } from "../client";
import { formatNumber, formatSeconds, lastValueFromColumns } from "./math";
import {
  colorOf,
  isSeriesVisible,
  legendReadoutTime,
  toHexColor,
  valueAt,
  type LegendSignalDom,
  type TimelineState,
} from "./state";

export interface LegendCallbacks {
  onSelectSignal: (item: SeriesInfo, trackName: string) => void;
  onToggleVisible: (item: SeriesInfo) => void;
  onChangeColor: (item: SeriesInfo, color: string) => void;
  onToggleSolo: (item: SeriesInfo) => void;
  onRemoveSignal: (item: SeriesInfo) => void;
  onToggleAutoY: (trackName: string) => void;
  onManualYRange: (trackName: string, min: number, max: number) => void;
  onToggleCollapse: (trackName: string) => void;
  onMoveSeriesToTrack: (seriesId: number, targetTrack: string | null) => void;
  onPlotRttSignal: (signalName: string, targetTrack: string | null) => void;
  onRemoveRttSeries: (seriesId: number) => void;
  onContextMenuSignal: (
    event: MouseEvent,
    trackName: string,
    signal: SeriesInfo,
  ) => void;
  onDragStateChange: (active: boolean) => void;
}

export function splitSignalName(name: string): { prefix: string; tail: string } {
  let i = name.length;
  // Walk backwards past any trailing `[...]` array subscripts so they stay in `tail`.
  while (i > 0 && name[i - 1] === "]") {
    let depth = 0;
    let j = i - 1;
    while (j >= 0) {
      if (name[j] === "]") depth++;
      else if (name[j] === "[") {
        depth--;
        if (depth === 0) break;
      }
      j--;
    }
    if (j <= 0) break;
    i = j;
  }
  const dot = name.lastIndexOf(".", i - 1);
  const arrow = name.lastIndexOf("->", i - 1);
  const scope = name.lastIndexOf("::", i - 1);
  let cut = -1;
  if (dot >= 0) cut = dot + 1;
  if (arrow >= 0 && arrow + 2 > cut) cut = arrow + 2;
  if (scope >= 0 && scope + 2 > cut) cut = scope + 2;
  if (cut <= 0 || cut >= name.length) {
    return { prefix: "", tail: name };
  }
  return { prefix: name.slice(0, cut), tail: name.slice(cut) };
}

export function describeModeBadge(item: SeriesInfo): {
  label: string;
  title: string;
  modeCls: string;
} {
  if (item.error) {
    return { label: "ERR", title: item.error, modeCls: "err" };
  }
  const mode = item.actualMode.toUpperCase();
  if (!item.rateHz) {
    return { label: mode, title: `Source: ${mode}`, modeCls: item.actualMode };
  }
  const rate = item.rateHz >= 1000 ? `${item.rateHz / 1000}k` : `${item.rateHz}`;
  return {
    label: `${mode} ${rate}`,
    title: `Source: ${mode} @ ${item.rateHz} Hz`,
    modeCls: item.actualMode,
  };
}

export function renderTrackLegendHeader(
  legendEl: HTMLElement,
  trackName: string,
  state: TimelineState,
  callbacks: LegendCallbacks,
  uniformBadge: { label: string; title: string; modeCls: string } | null = null,
): {
  yAutoBtn: HTMLButtonElement;
  yRangeWrap: HTMLElement;
  yMinInput: HTMLInputElement;
  yMaxInput: HTMLInputElement;
} {
  const yCfg = state.groupYScales.get(trackName) ?? {
    auto: true,
    min: -1,
    max: 1,
    collapsed: false,
  };

  const head = document.createElement("div");
  head.className = "track-legend-header";

  const collapseBtn = document.createElement("button");
  collapseBtn.type = "button";
  collapseBtn.className = "mini-btn track-collapse-btn";
  collapseBtn.title = "Collapse / expand track";
  collapseBtn.textContent = yCfg.collapsed ? "▸" : "▾";
  collapseBtn.onclick = (e) => {
    e.stopPropagation();
    callbacks.onToggleCollapse(trackName);
  };

  const title = document.createElement("span");
  title.className = "track-title";
  title.textContent = trackName;
  title.title = trackName;

  const yAutoBtn = document.createElement("button");
  yAutoBtn.type = "button";
  yAutoBtn.className = "mini-btn";
  yAutoBtn.title = "Toggle Auto Y vs Locked Y (or press Y)";
  yAutoBtn.textContent = yCfg.auto ? "Auto Y" : "Lock Y";
  yAutoBtn.setAttribute("aria-pressed", String(!yCfg.auto));
  yAutoBtn.onclick = (e) => {
    e.stopPropagation();
    callbacks.onToggleAutoY(trackName);
  };

  if (uniformBadge) {
    const trackBadge = document.createElement("span");
    trackBadge.className = `source-badge ${uniformBadge.modeCls} track-rate-badge`;
    trackBadge.textContent = uniformBadge.label;
    trackBadge.title = uniformBadge.title;
    head.append(collapseBtn, title, trackBadge, yAutoBtn);
  } else {
    head.append(collapseBtn, title, yAutoBtn);
  }

  const yRangeWrap = document.createElement("div");
  yRangeWrap.className = "track-y-range";
  yRangeWrap.hidden = yCfg.auto;

  const yMinInput = document.createElement("input");
  yMinInput.type = "number";
  yMinInput.step = "any";
  yMinInput.title = "Y minimum";
  yMinInput.value = formatNumber(yCfg.min, 4);

  const sep = document.createElement("span");
  sep.textContent = "..";

  const yMaxInput = document.createElement("input");
  yMaxInput.type = "number";
  yMaxInput.step = "any";
  yMaxInput.title = "Y maximum";
  yMaxInput.value = formatNumber(yCfg.max, 4);

  const commit = () => {
    const lo = Number(yMinInput.value);
    const hi = Number(yMaxInput.value);
    if (Number.isFinite(lo) && Number.isFinite(hi) && hi > lo) {
      callbacks.onManualYRange(trackName, lo, hi);
    }
  };
  yMinInput.onchange = commit;
  yMaxInput.onchange = commit;
  yRangeWrap.append(yMinInput, sep, yMaxInput);

  legendEl.append(head, yRangeWrap);
  return { yAutoBtn, yRangeWrap, yMinInput, yMaxInput };
}

export function renderTrackLegendSignals(
  legendEl: HTMLElement,
  trackName: string,
  members: SeriesInfo[],
  state: TimelineState,
  callbacks: LegendCallbacks,
  hideUniformBadges = false,
): {
  legendSignals: Map<number, LegendSignalDom>;
  legendValues: Map<number, HTMLElement>;
  legendCards: Map<number, HTMLElement>;
} {
  const list = document.createElement("div");
  list.className = "track-signal-list";
  const legendSignals = new Map<number, LegendSignalDom>();
  const legendValues = new Map<number, HTMLElement>();
  const legendCards = new Map<number, HTMLElement>();

  const filter = state.seriesFilter.trim().toLowerCase();

  for (const item of members) {
    const color = colorOf(state, item);
    const visible = isSeriesVisible(state, item);
    const selected = state.selectedSeriesId === item.seriesId;

    const row = document.createElement("div");
    row.className =
      "legend-signal-row" +
      (visible ? "" : " is-hidden") +
      (selected ? " is-selected" : "") +
      (item.error ? " is-broken" : "");
    if (filter) {
      const hay = `${item.name} ${item.root} ${item.group ?? ""} ${item.actualMode}`.toLowerCase();
      if (!hay.includes(filter)) {
        row.classList.add("is-filtered-out");
      }
    }
    row.draggable = true;
    row.title = item.error
      ? `Error: ${item.error}\nExpr: ${item.root}`
      : `${item.root}\nDrag to move to another track`;

    row.addEventListener("click", () => {
      callbacks.onSelectSignal(item, trackName);
    });

    row.addEventListener("contextmenu", (event) => {
      event.preventDefault();
      event.stopPropagation();
      callbacks.onSelectSignal(item, trackName);
      callbacks.onContextMenuSignal(event, trackName, item);
    });

    row.addEventListener("dragstart", (event) => {
      state.dragging = { kind: "series", seriesId: item.seriesId };
      if (event.dataTransfer) {
        event.dataTransfer.effectAllowed = "move";
        event.dataTransfer.setData("text/plain", `series:${item.seriesId}`);
      }
      callbacks.onDragStateChange(true);
    });
    row.addEventListener("dragend", () => {
      state.dragging = null;
      callbacks.onDragStateChange(false);
    });

    const visToggle = document.createElement("input");
    visToggle.type = "checkbox";
    visToggle.checked = visible;
    visToggle.style.accentColor = color;
    visToggle.title = "Show / hide signal (H)";
    visToggle.onclick = (e) => e.stopPropagation();
    visToggle.onchange = () => callbacks.onToggleVisible(item);

    const swatch = document.createElement("input");
    swatch.type = "color";
    swatch.className = "legend-swatch";
    swatch.value = toHexColor(color);
    swatch.title = "Change signal color";
    swatch.onclick = (e) => e.stopPropagation();
    swatch.onchange = () => callbacks.onChangeColor(item, swatch.value);

    const nameEl = document.createElement("span");
    nameEl.className = "legend-sig-name mono";
    const { prefix, tail } = splitSignalName(item.name);
    if (prefix) {
      const prefixEl = document.createElement("span");
      prefixEl.className = "legend-sig-prefix";
      prefixEl.textContent = prefix;
      const tailEl = document.createElement("span");
      tailEl.className = "legend-sig-tail";
      tailEl.textContent = tail;
      nameEl.append(prefixEl, tailEl);
    } else {
      const tailEl = document.createElement("span");
      tailEl.className = "legend-sig-tail";
      tailEl.textContent = tail;
      nameEl.append(tailEl);
    }

    const badgeInfo = describeModeBadge(item);
    const showRowBadge = !hideUniformBadges || Boolean(item.error);
    const badge = showRowBadge ? document.createElement("span") : null;
    if (badge) {
      badge.className = `source-badge ${badgeInfo.modeCls}`;
      badge.textContent = badgeInfo.label;
      badge.title = badgeInfo.title;
    }

    const valEl = document.createElement("span");
    valEl.className = "legend-sig-val mono";
    valEl.textContent = "—";
    legendValues.set(item.seriesId, valEl);

    const soloBtn = document.createElement("button");
    soloBtn.type = "button";
    soloBtn.className =
      "mini-icon-btn solo-btn" + (state.soloSeriesId === item.seriesId ? " active" : "");
    soloBtn.textContent = "S";
    soloBtn.title = "Solo signal (S)";
    soloBtn.onclick = (e) => {
      e.stopPropagation();
      callbacks.onToggleSolo(item);
    };

    const rmBtn = document.createElement("button");
    rmBtn.type = "button";
    rmBtn.className = "mini-icon-btn remove-btn";
    rmBtn.textContent = "×";
    rmBtn.title = "Remove signal (Del)";
    rmBtn.onclick = (e) => {
      e.stopPropagation();
      callbacks.onRemoveSignal(item);
    };

    const actions = document.createElement("span");
    actions.className = "legend-sig-actions";
    actions.append(soloBtn, rmBtn);

    if (badge) {
      row.append(visToggle, swatch, nameEl, badge, valEl, actions);
    } else {
      row.append(visToggle, swatch, nameEl, valEl, actions);
    }
    legendCards.set(item.seriesId, row);
    legendSignals.set(item.seriesId, {
      row,
      checkbox: visToggle,
      swatch,
      soloBtn,
      valueEl: valEl,
      lastText: "—",
    });
    list.append(row);
  }

  legendEl.append(list);
  return { legendSignals, legendValues, legendCards };
}

let cachedHoverEl: HTMLElement | null = null;
let lastHoverText = "";

export function updateLegendReadouts(state: TimelineState): void {
  if (!cachedHoverEl) {
    cachedHoverEl = document.getElementById("hover-readout");
  }
  const tReadout = legendReadoutTime(state);
  if (cachedHoverEl) {
    let text = "";
    if (state.hoverTime !== null) {
      text = `Hover t = ${state.hoverTime.toFixed(4)} s`;
      if (state.sampleCursor !== null) {
        const dt = state.hoverTime - state.sampleCursor;
        text += `  Δt = ${formatSeconds(dt)}`;
        const selected =
          state.selectedSeriesId !== null
            ? state.series.find((s) => s.seriesId === state.selectedSeriesId)
            : undefined;
        if (selected) {
          const vHover = valueAt(state, selected, state.hoverTime);
          const vSample = valueAt(state, selected, state.sampleCursor);
          if (vHover !== null && vSample !== null) {
            text += `  Δ${selected.name} = ${formatNumber(vHover - vSample, 5)}`;
          }
        }
      }
    } else if (state.sampleCursor !== null) {
      text = `Sample S = ${state.sampleCursor.toFixed(4)} s`;
    }
    if (text !== lastHoverText) {
      lastHoverText = text;
      cachedHoverEl.textContent = text;
    }
  }

  const seriesCols = state.lastColumns?.result.series;
  for (const track of state.tracks) {
    for (const item of track.members) {
      const sigDom = track.legendSignals.get(item.seriesId);
      if (!sigDom) continue;
      let val: number | null = null;
      if (tReadout !== null) {
        val = valueAt(state, item, tReadout);
      } else {
        val = state.streamBuffer.lastValue(item.seriesId);
        if (val === null && seriesCols) {
          val = lastValueFromColumns(seriesCols.get(item.seriesId));
        }
      }
      const formatted = formatNumber(val, 6);
      if (formatted !== sigDom.lastText) {
        sigDom.lastText = formatted;
        sigDom.valueEl.textContent = formatted;
      }
    }
  }
}

let rttStructureKey = "";
const rttValueEls = new Map<string, HTMLElement>();

export function renderRttSignalsDrawer(
  state: TimelineState,
  callbacks: LegendCallbacks,
): void {
  const countBadge = document.getElementById("rtt-badge-count");
  const countText = String(state.rttSignals.length);
  if (countBadge && countBadge.textContent !== countText) {
    countBadge.textContent = countText;
  }

  const listEl = document.getElementById("rtt-signal-list");
  if (!listEl) return;

  if (state.rttSignals.length === 0) {
    if (rttStructureKey !== "__empty__") {
      rttStructureKey = "__empty__";
      rttValueEls.clear();
      const empty = document.createElement("div");
      empty.className = "drawer-empty";
      empty.textContent = "No decoded RTT signals yet.";
      listEl.replaceChildren(empty);
    }
    return;
  }

  const nextKey = state.rttSignals
    .map((sig) => `${sig.name}:${sig.seriesId ?? ""}`)
    .join("|");

  if (rttStructureKey !== nextKey) {
    rttStructureKey = nextKey;
    rttValueEls.clear();
    listEl.replaceChildren(
      ...state.rttSignals.map((sig) => {
        const row = document.createElement("div");
        row.className = "rtt-sig-row";
        row.draggable = true;
        row.title = "Drag onto a track (or blank area) to plot, or click + Plot";

        row.addEventListener("dragstart", (event) => {
          state.dragging = { kind: "rtt", signalName: sig.name };
          if (event.dataTransfer) {
            event.dataTransfer.effectAllowed = "copyMove";
            event.dataTransfer.setData("text/plain", `rtt:${sig.name}`);
          }
          callbacks.onDragStateChange(true);
        });
        row.addEventListener("dragend", () => {
          state.dragging = null;
          callbacks.onDragStateChange(false);
        });

        const nameEl = document.createElement("span");
        nameEl.className = "mono rtt-sig-name";
        nameEl.textContent = sig.name;

        const valEl = document.createElement("span");
        valEl.className = "mono rtt-sig-val";
        valEl.textContent = sig.value !== null ? formatNumber(sig.value, 5) : "—";
        rttValueEls.set(sig.name, valEl);

        const btn = document.createElement("button");
        btn.type = "button";
        btn.className = "mini-btn";
        btn.textContent = sig.seriesId === null ? "+ Plot" : "Remove";
        btn.onclick = () => {
          if (sig.seriesId === null) {
            callbacks.onPlotRttSignal(sig.name, null);
          } else {
            callbacks.onRemoveRttSeries(sig.seriesId);
          }
        };

        row.append(nameEl, valEl, btn);
        return row;
      }),
    );
    return;
  }

  const drawer = document.getElementById("bottom-drawer");
  const rttPane = document.getElementById("drawer-rtt");
  if (!drawer || drawer.hidden || !rttPane || rttPane.hidden) {
    return;
  }

  for (const sig of state.rttSignals) {
    const valEl = rttValueEls.get(sig.name);
    if (!valEl) continue;
    const text = sig.value !== null ? formatNumber(sig.value, 5) : "—";
    if (valEl.textContent !== text) {
      valEl.textContent = text;
    }
  }
}
