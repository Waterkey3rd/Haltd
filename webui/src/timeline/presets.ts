import type { Client, SeriesInfo } from "../client";
import {
  DEFAULT_TRACK_HEIGHT,
  effectiveGroupOf,
  getGroupYScale,
  saveTrackHeights,
  type TimelineState,
} from "./state";
import { syncDivisionControls } from "./toolbar";

export interface PresetSignal {
  kind: "expr" | "rtt";
  expr: string;
  mode: string;
  group: string | null;
  color: string | null;
  hidden: boolean;
}

export interface PresetTrack {
  autoY: boolean;
  yMin: number | null;
  yMax: number | null;
  heightPx: number | null;
  collapsed: boolean;
}

export interface TimelinePreset {
  name: string;
  updatedAt: string;
  rateHz: number | null;
  division: number | null;
  splitLanes: boolean;
  showBands: boolean;
  signals: PresetSignal[];
  tracks: Record<string, PresetTrack>;
}

export interface TimelinePresetFile {
  version: number;
  activePreset: string | null;
  presets: TimelinePreset[];
}

export function buildPresetFromState(
  name: string,
  state: TimelineState,
  rateHz: number | null,
): TimelinePreset {
  const signals: PresetSignal[] = state.series.map((item) => {
    const isRtt = item.actualMode === "rtt" || item.mode === "rtt";
    return {
      kind: isRtt ? "rtt" : "expr",
      expr: item.name,
      mode: isRtt ? "rtt" : (item.mode ?? "auto"),
      group: item.group ?? null,
      color: item.color ?? null,
      hidden: state.hidden.has(item.seriesId),
    };
  });

  const tracks: Record<string, PresetTrack> = {};
  const trackNames = new Set<string>(state.series.map((item) => effectiveGroupOf(state, item)));
  for (const trackName of [...trackNames].sort()) {
    const cfg = getGroupYScale(state, trackName);
    const trackObj = state.tracks.find((t) => t.name === trackName);
    const heightPx =
      trackObj?.heightPx ?? state.trackHeights.get(trackName) ?? DEFAULT_TRACK_HEIGHT;
    tracks[trackName] = {
      autoY: cfg.auto,
      yMin: cfg.auto ? null : cfg.min,
      yMax: cfg.auto ? null : cfg.max,
      heightPx: Math.round(heightPx),
      collapsed: cfg.collapsed,
    };
  }

  return {
    name,
    updatedAt: new Date().toISOString(),
    rateHz,
    division: state.division,
    splitLanes: state.splitLanes,
    showBands: state.showBands,
    signals,
    tracks,
  };
}

export function presetSignature(preset: TimelinePreset): string {
  const sortedTracks = Object.keys(preset.tracks)
    .sort()
    .map((key) => {
      const t = preset.tracks[key];
      return [
        key,
        {
          autoY: t.autoY,
          yMin: t.yMin,
          yMax: t.yMax,
          heightPx: t.heightPx,
          collapsed: t.collapsed,
        },
      ];
    });
  return JSON.stringify({
    rateHz: preset.rateHz,
    splitLanes: preset.splitLanes,
    showBands: preset.showBands,
    signals: preset.signals.map((s) => ({
      kind: s.kind,
      expr: s.expr,
      mode: s.mode,
      group: s.group,
      color: s.color,
      hidden: s.hidden,
    })),
    tracks: sortedTracks,
  });
}

export function isPresetDirty(state: TimelineState, rateHz: number | null): boolean {
  if (!state.activePresetName || !state.savedPresetSnapshot) {
    return false;
  }
  const current = buildPresetFromState(state.activePresetName, state, rateHz);
  return presetSignature(current) !== state.savedPresetSnapshot;
}

export interface PresetUiCallbacks {
  loadSeries: () => Promise<void>;
  refresh: () => Promise<void>;
  rebuild: () => void;
  showRateReply: (rate: { rateHz: number; maxHz: number }) => void;
  setMeasureText: (text: string) => void;
}

function createMonoSvg(pathD: string): SVGSVGElement {
  const svg = document.createElementNS("http://www.w3.org/2000/svg", "svg");
  svg.setAttribute("width", "12");
  svg.setAttribute("height", "12");
  svg.setAttribute("viewBox", "0 0 16 16");
  svg.setAttribute("aria-hidden", "true");
  const path = document.createElementNS("http://www.w3.org/2000/svg", "path");
  path.setAttribute("fill", "currentColor");
  path.setAttribute("d", pathD);
  svg.append(path);
  return svg;
}

const SVG_EDIT_PATH =
  "M12.15 2.15a1.5 1.5 0 0 1 2.12 2.12l-7.9 7.9-2.83.71.71-2.83 7.9-7.9zm1.06 1.06a.5.5 0 0 0-.71 0l-.88.88.71.71.88-.88a.5.5 0 0 0 0-.71zm-1.41 1.41l-6.6 6.6-.29 1.15 1.15-.29 6.6-6.6-.86-.86z";
const SVG_TRASH_PATH =
  "M5.5 2a.5.5 0 0 1 .5-.5h4a.5.5 0 0 1 .5.5v1h3a.5.5 0 0 1 0 1h-.6l-.7 8.6a1.5 1.5 0 0 1-1.5 1.4H5.3a1.5 1.5 0 0 1-1.5-1.4L3.1 4H2.5a.5.5 0 0 1 0-1h3V2zm1 1h3V2.5h-3V3zm-2.4 1l.68 8.5a.5.5 0 0 0 .5.47h5.44a.5.5 0 0 0 .5-.47L11.9 4H4.1z";

export class PresetController {
  private presets: TimelinePreset[] = [];
  private activePreset: string | null = null;
  private renamingName: string | null = null;

  constructor(
    private readonly client: Client,
    private readonly state: TimelineState,
    private readonly callbacks: PresetUiCallbacks,
  ) {
    this.wireDom();
  }

  private currentRateHz(): number | null {
    const rateEl = document.getElementById("rate") as HTMLSelectElement | null;
    if (!rateEl || !rateEl.value) return null;
    const val = Number(rateEl.value);
    return Number.isFinite(val) ? val : null;
  }

  async refreshFromServer(): Promise<void> {
    try {
      const file = await this.client.call<TimelinePresetFile>("timeline.presets.list");
      this.applyFile(file);
    } catch {
      // Ignore if backend unavailable yet
    }
  }

  syncDirtyIndicator(): void {
    const btn = document.getElementById("preset-btn");
    const label = document.getElementById("preset-label");
    const dirtyDot = document.getElementById("preset-dirty");
    if (label) {
      label.textContent = this.state.activePresetName ?? "Presets";
    }
    const dirty = isPresetDirty(this.state, this.currentRateHz());
    if (dirtyDot) {
      dirtyDot.hidden = !dirty;
    }
    if (btn) {
      btn.title = this.state.activePresetName
        ? `Timeline preset: ${this.state.activePresetName}${dirty ? " (modified)" : ""}`
        : "Load or save workspace Timeline presets";
    }
  }

  renderEmptyQuickLaunch(): void {
    const wrap = document.getElementById("empty-presets");
    if (!wrap) return;
    if (this.presets.length === 0) {
      wrap.hidden = true;
      wrap.replaceChildren();
      return;
    }
    wrap.hidden = false;
    const title = document.createElement("span");
    title.className = "empty-presets-label";
    title.textContent = "Workspace Presets:";
    const chips = this.presets.slice(0, 8).map((preset) => {
      const btn = document.createElement("button");
      btn.type = "button";
      btn.className = "mini-btn empty-preset-chip";
      const rateSuffix = preset.rateHz !== null ? ` · ${preset.rateHz} Hz` : "";
      btn.textContent = `${preset.name} (${preset.signals.length}${rateSuffix})`;
      btn.addEventListener("click", () => {
        void this.applyPreset(preset, true);
      });
      return btn;
    });
    wrap.replaceChildren(title, ...chips);
  }

  closePopover(): void {
    const pop = document.getElementById("preset-popover");
    if (pop) pop.hidden = true;
    this.renamingName = null;
  }

  private applyFile(file: TimelinePresetFile): void {
    this.presets = file.presets ?? [];
    this.activePreset = file.activePreset ?? null;
    this.state.activePresetName = this.activePreset;
    if (this.activePreset && !this.state.savedPresetSnapshot) {
      const match = this.presets.find((p) => p.name === this.activePreset);
      if (match) {
        this.state.savedPresetSnapshot = presetSignature(match);
      }
    }
    this.syncDirtyIndicator();
    this.renderPopoverList();
    this.renderEmptyQuickLaunch();
  }

  async applyPreset(preset: TimelinePreset, replace: boolean): Promise<void> {
    this.callbacks.setMeasureText(
      `${replace ? "Loading" : "Appending"} preset "${preset.name}"…`,
    );
    try {
      const reply = await this.client.call<{ series: SeriesInfo[] }>("timeline.applyBatch", {
        replace,
        ...(replace && preset.rateHz !== null ? { rateHz: preset.rateHz } : {}),
        signals: preset.signals.map((sig) => ({
          kind: sig.kind,
          expr: sig.expr,
          mode: sig.mode,
          group: sig.group,
          color: sig.color,
        })),
      });

      if (replace) {
        this.state.soloSeriesId = null;
        this.state.selectedSeriesId = null;
        this.state.selectedTrack = null;
        this.state.hidden.clear();
        this.state.splitLanes = preset.splitLanes;
        this.state.showBands = preset.showBands;
        if (preset.division && preset.division > 0) {
          this.state.division = preset.division;
        }
        const layoutBtn = document.getElementById("layout-mode") as HTMLButtonElement | null;
        if (layoutBtn) {
          layoutBtn.textContent = this.state.splitLanes ? "Split" : "Grouped";
          layoutBtn.setAttribute("aria-pressed", String(this.state.splitLanes));
        }
        document
          .getElementById("toggle-band")
          ?.setAttribute("aria-pressed", String(this.state.showBands));
        syncDivisionControls(this.state);

        if (preset.rateHz !== null) {
          const rate = await this.client.call<{ rateHz: number; maxHz: number }>("timeline.rate");
          this.callbacks.showRateReply(rate);
        }

        // Restore per-track Y scale, height, and collapse state before rebuild.
        this.state.groupYScales.clear();
        for (const [trackName, cfg] of Object.entries(preset.tracks ?? {})) {
          this.state.groupYScales.set(trackName, {
            auto: cfg.autoY,
            min: cfg.yMin ?? -1,
            max: cfg.yMax ?? 1,
            collapsed: cfg.collapsed,
          });
          if (cfg.heightPx && cfg.heightPx >= 80) {
            this.state.trackHeights.set(trackName, cfg.heightPx);
          }
        }
        saveTrackHeights(this.state.trackHeights);

        const file = await this.client.call<TimelinePresetFile>("timeline.presets.setActive", {
          name: preset.name,
        });
        this.activePreset = file.activePreset;
        this.state.activePresetName = preset.name;
        this.state.savedPresetSnapshot = presetSignature(preset);
      }

      await this.callbacks.loadSeries();

      // Match newly loaded series against preset hidden flags & track configs.
      if (replace) {
        const hiddenExprs = new Set(preset.signals.filter((s) => s.hidden).map((s) => s.expr));
        for (const series of this.state.series) {
          if (hiddenExprs.has(series.name)) {
            this.state.hidden.add(series.seriesId);
          }
        }
        this.callbacks.rebuild();
      } else {
        const returned = reply.series ?? [];
        const hiddenExprs = new Set(preset.signals.filter((s) => s.hidden).map((s) => s.expr));
        for (const s of returned) {
          if (hiddenExprs.has(s.name)) {
            this.state.hidden.add(s.seriesId);
          }
        }
        this.callbacks.rebuild();
      }

      await this.callbacks.refresh();
      const failedCount = (reply.series ?? []).filter((s) => Boolean(s.error)).length;
      if (replace && failedCount === 0) {
        this.state.savedPresetSnapshot = presetSignature(
          buildPresetFromState(preset.name, this.state, this.currentRateHz()),
        );
      }
      if (failedCount > 0) {
        this.callbacks.setMeasureText(
          `Preset "${preset.name}" applied (${failedCount} signal(s) unresolved in current ELF).`,
        );
      } else {
        this.callbacks.setMeasureText(
          `Preset "${preset.name}" ${replace ? "loaded" : "appended"} (${preset.signals.length} signals).`,
        );
      }
      this.closePopover();
      this.syncDirtyIndicator();
      this.renderPopoverList();
    } catch (error) {
      this.callbacks.setMeasureText(String(error));
    }
  }

  async savePresetAs(name: string, overwriteName?: string): Promise<void> {
    const trimmed = name.trim();
    if (!trimmed) {
      this.callbacks.setMeasureText("Enter a preset name first.");
      return;
    }
    const preset = buildPresetFromState(trimmed, this.state, this.currentRateHz());
    try {
      const file = await this.client.call<TimelinePresetFile>("timeline.presets.save", {
        preset,
        ...(overwriteName ? { overwriteName } : {}),
      });
      this.state.activePresetName = trimmed;
      this.state.savedPresetSnapshot = presetSignature(preset);
      this.applyFile(file);
      this.callbacks.setMeasureText(`Saved preset "${trimmed}" (${preset.signals.length} signals).`);
    } catch (error) {
      this.callbacks.setMeasureText(String(error));
    }
  }

  private async deletePreset(name: string): Promise<void> {
    try {
      const file = await this.client.call<TimelinePresetFile>("timeline.presets.delete", { name });
      if (this.state.activePresetName === name) {
        this.state.activePresetName = null;
        this.state.savedPresetSnapshot = null;
      }
      this.applyFile(file);
      this.callbacks.setMeasureText(`Deleted preset "${name}".`);
    } catch (error) {
      this.callbacks.setMeasureText(String(error));
    }
  }

  private async renamePreset(from: string, to: string): Promise<void> {
    const trimmed = to.trim();
    if (!trimmed || trimmed === from) {
      this.renamingName = null;
      this.renderPopoverList();
      return;
    }
    try {
      const file = await this.client.call<TimelinePresetFile>("timeline.presets.rename", {
        from,
        to: trimmed,
      });
      if (this.state.activePresetName === from) {
        this.state.activePresetName = trimmed;
      }
      this.renamingName = null;
      this.applyFile(file);
      this.callbacks.setMeasureText(`Renamed preset "${from}" -> "${trimmed}".`);
    } catch (error) {
      this.callbacks.setMeasureText(String(error));
    }
  }

  private openPopover(focusInput = false): void {
    const pop = document.getElementById("preset-popover");
    const btn = document.getElementById("preset-btn");
    if (!pop || !btn) return;

    const exportPop = document.getElementById("export-popover");
    if (exportPop) exportPop.hidden = true;
    const eventsPop = document.getElementById("events-popover");
    if (eventsPop) eventsPop.hidden = true;

    const rect = btn.getBoundingClientRect();
    pop.style.left = `${Math.max(8, Math.min(rect.left, window.innerWidth - 348))}px`;
    pop.style.top = `${rect.bottom + 4}px`;
    pop.style.right = "auto";
    pop.style.bottom = "auto";
    pop.hidden = false;

    this.renderPopoverList();
    if (focusInput) {
      const input = document.getElementById("preset-new-name") as HTMLInputElement | null;
      if (input) {
        input.value = this.state.activePresetName ?? "";
        input.focus();
        input.select();
      }
    }
  }

  private renderPopoverList(): void {
    const list = document.getElementById("preset-list");
    if (!list) return;

    if (this.presets.length === 0) {
      const empty = document.createElement("div");
      empty.className = "drawer-empty";
      empty.textContent = "No workspace presets yet. Save current signals below.";
      list.replaceChildren(empty);
      return;
    }

    const items = this.presets.map((preset) => {
      const row = document.createElement("div");
      row.className = "preset-row";
      if (preset.name === this.activePreset) {
        row.classList.add("is-active");
      }

      if (this.renamingName === preset.name) {
        const input = document.createElement("input");
        input.type = "text";
        input.className = "preset-rename-input";
        input.value = preset.name;
        const commit = () => void this.renamePreset(preset.name, input.value);
        input.addEventListener("keydown", (e) => {
          if (e.key === "Enter") {
            e.preventDefault();
            commit();
          } else if (e.key === "Escape") {
            e.preventDefault();
            this.renamingName = null;
            this.renderPopoverList();
          }
        });
        input.addEventListener("blur", commit);
        row.append(input);
        queueMicrotask(() => {
          input.focus();
          input.select();
        });
        return row;
      }

      const mainBtn = document.createElement("button");
      mainBtn.type = "button";
      mainBtn.className = "preset-row-main";
      mainBtn.title = `Load "${preset.name}" (replace current signals)`;
      const nameSpan = document.createElement("span");
      nameSpan.className = "preset-row-name";
      nameSpan.textContent = preset.name;
      const metaSpan = document.createElement("span");
      metaSpan.className = "preset-row-meta";
      const ratePart = preset.rateHz !== null ? ` · ${preset.rateHz}Hz` : "";
      metaSpan.textContent = `${preset.signals.length} sig${ratePart}`;
      mainBtn.append(nameSpan, metaSpan);
      mainBtn.addEventListener("click", () => {
        void this.applyPreset(preset, true);
      });

      const appendBtn = document.createElement("button");
      appendBtn.type = "button";
      appendBtn.className = "mini-btn preset-row-act";
      appendBtn.textContent = "+";
      appendBtn.title = `Append signals from "${preset.name}" without clearing`;
      appendBtn.addEventListener("click", (e) => {
        e.stopPropagation();
        void this.applyPreset(preset, false);
      });

      const renameBtn = document.createElement("button");
      renameBtn.type = "button";
      renameBtn.className = "mini-btn preset-row-act";
      renameBtn.title = `Rename "${preset.name}"`;
      renameBtn.append(createMonoSvg(SVG_EDIT_PATH));
      renameBtn.addEventListener("click", (e) => {
        e.stopPropagation();
        this.renamingName = preset.name;
        this.renderPopoverList();
      });

      const delBtn = document.createElement("button");
      delBtn.type = "button";
      delBtn.className = "mini-btn preset-row-act danger-hover";
      delBtn.title = `Delete "${preset.name}"`;
      delBtn.append(createMonoSvg(SVG_TRASH_PATH));
      delBtn.addEventListener("click", (e) => {
        e.stopPropagation();
        void this.deletePreset(preset.name);
      });

      row.append(mainBtn, appendBtn, renameBtn, delBtn);
      return row;
    });

    list.replaceChildren(...items);
  }

  private wireDom(): void {
    const btn = document.getElementById("preset-btn");
    const saveBtn = document.getElementById("preset-save");
    const closeBtn = document.getElementById("preset-close");
    const saveForm = document.getElementById("preset-save-form") as HTMLFormElement | null;
    const nameInput = document.getElementById("preset-new-name") as HTMLInputElement | null;
    const pop = document.getElementById("preset-popover");

    btn?.addEventListener("click", () => {
      if (pop && !pop.hidden) {
        this.closePopover();
      } else {
        this.openPopover(false);
        void this.refreshFromServer();
      }
    });

    saveBtn?.addEventListener("click", () => {
      if (this.state.activePresetName) {
        void this.savePresetAs(this.state.activePresetName, this.state.activePresetName);
      } else {
        this.openPopover(true);
      }
    });

    closeBtn?.addEventListener("click", () => this.closePopover());

    saveForm?.addEventListener("submit", (event) => {
      event.preventDefault();
      if (!nameInput) return;
      void this.savePresetAs(nameInput.value);
      nameInput.value = "";
    });

    window.addEventListener("mousedown", (event) => {
      if (!pop || pop.hidden) return;
      const target = event.target as Node;
      if (!pop.contains(target) && btn && !btn.contains(target) && saveBtn && !saveBtn.contains(target)) {
        this.closePopover();
      }
    });

    window.addEventListener("focus", () => {
      void this.refreshFromServer();
    });
  }
}
