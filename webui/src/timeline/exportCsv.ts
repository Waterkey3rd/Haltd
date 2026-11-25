import { spanSeconds } from "./math";
import { isSeriesVisible, nowSeconds, type TimelineState } from "./state";

export type ExportRangeMode = "cursors" | "visible" | "all" | "recording";

export interface CsvExportParams {
  seriesIds?: number[];
  seriesNames?: string[];
  t0?: number;
  t1?: number;
  zoh: boolean;
  relativeTime: boolean;
  fromRecording?: boolean;
  hlrecPath?: string;
}

export interface SaveExportBridgeRequest {
  kind: "csv" | "hlrec" | "png";
  defaultFilename: string;
  params?: Record<string, unknown>;
  dataUrl?: string;
}

export interface SaveExportResult {
  ok: boolean;
  cancelled?: boolean;
  message?: string;
  path?: string;
}

export function defaultExportRangeMode(
  state: Pick<TimelineState, "cursorA" | "cursorB">,
): ExportRangeMode {
  return state.cursorA !== null && state.cursorB !== null ? "cursors" : "visible";
}

export function resolveTimeBoundsNs(
  mode: ExportRangeMode,
  state: Pick<TimelineState, "cursorA" | "cursorB" | "followLive" | "viewEnd" | "division">,
  currentTimeSec: number,
): { t0?: number; t1?: number } {
  if (mode === "cursors" && state.cursorA !== null && state.cursorB !== null) {
    const lo = Math.min(state.cursorA, state.cursorB);
    const hi = Math.max(state.cursorA, state.cursorB);
    return { t0: Math.round(lo * 1e9), t1: Math.round(hi * 1e9) };
  }
  if (mode === "visible") {
    const end = state.followLive ? currentTimeSec : state.viewEnd;
    const span = spanSeconds(state.division);
    return { t0: Math.round((end - span) * 1e9), t1: Math.round(end * 1e9) };
  }
  return {};
}

export function defaultExportFilename(
  activePresetName: string | null,
  ext: "csv" | "hlrec" | "png",
  now: Date = new Date(),
): string {
  const raw = (activePresetName ?? "").trim();
  const stem = raw.replace(/[^A-Za-z0-9._-]+/g, "_").replace(/^_+|_+$/g, "") || "timeline";
  const pad = (n: number) => String(n).padStart(2, "0");
  const stamp = `${now.getFullYear()}${pad(now.getMonth() + 1)}${pad(now.getDate())}_${pad(now.getHours())}${pad(now.getMinutes())}${pad(now.getSeconds())}`;
  return `${stem}_${stamp}.${ext}`;
}

export function buildExportCsvUrl(params: CsvExportParams, filename: string): string {
  const qs = new URLSearchParams();
  if (params.seriesIds && params.seriesIds.length > 0) {
    qs.set("seriesIds", params.seriesIds.join(","));
  }
  if (params.seriesNames && params.seriesNames.length > 0) {
    qs.set("seriesNames", params.seriesNames.join(","));
  }
  if (params.t0 !== undefined) qs.set("t0", String(params.t0));
  if (params.t1 !== undefined) qs.set("t1", String(params.t1));
  qs.set("zoh", String(params.zoh));
  qs.set("relativeTime", String(params.relativeTime));
  if (params.fromRecording) qs.set("fromRecording", "true");
  if (params.hlrecPath) qs.set("hlrecPath", params.hlrecPath);
  if (filename) qs.set("filename", filename);
  return `/api/export.csv?${qs.toString()}`;
}

export function buildRecordingHlrecUrl(hlrecPath: string | undefined, filename: string): string {
  const qs = new URLSearchParams();
  if (hlrecPath) qs.set("hlrecPath", hlrecPath);
  if (filename) qs.set("filename", filename);
  return `/api/recording.hlrec?${qs.toString()}`;
}

let bridgeSeq = 1;

export async function saveExportViaHostOrBrowser(
  req: SaveExportBridgeRequest,
): Promise<SaveExportResult> {
  if (typeof window === "undefined") {
    return { ok: false, message: "browser environment unavailable" };
  }

  // Hosted inside VS Code BackendPanel iframe: delegate native Save As dialog to extension host.
  if (window.parent && window.parent !== window) {
    const requestId = `exp_${Date.now()}_${bridgeSeq++}`;
    return await new Promise<SaveExportResult>((resolve) => {
      const cleanupTimeoutMs = 300_000;
      const timer = window.setTimeout(() => {
        window.removeEventListener("message", onMessage);
        resolve({ ok: false, message: "save dialog timed out" });
      }, cleanupTimeoutMs);

      const onMessage = (event: MessageEvent<unknown>) => {
        const msg = event.data as
          | {
              type?: string;
              requestId?: string;
              ok?: boolean;
              message?: string;
              path?: string;
            }
          | null;
        if (!msg || msg.type !== "haltd:save-export-result" || msg.requestId !== requestId) {
          return;
        }
        window.clearTimeout(timer);
        window.removeEventListener("message", onMessage);
        resolve({
          ok: Boolean(msg.ok),
          cancelled: !msg.ok && msg.message === "cancelled",
          message: msg.message,
          path: msg.path,
        });
      };

      window.addEventListener("message", onMessage);
      window.parent.postMessage(
        {
          type: "haltd:save-export",
          requestId,
          kind: req.kind,
          defaultFilename: req.defaultFilename,
          ...(req.params ? { params: req.params } : {}),
          ...(req.dataUrl ? { dataUrl: req.dataUrl } : {}),
        },
        "*",
      );
    });
  }

  // Standalone browser: trigger direct attachment download.
  const link = document.createElement("a");
  link.download = req.defaultFilename;
  if (req.kind === "png") {
    if (!req.dataUrl) return { ok: false, message: "missing PNG data" };
    link.href = req.dataUrl;
  } else if (req.kind === "hlrec") {
    const hlrecPath =
      typeof req.params?.hlrecPath === "string" ? req.params.hlrecPath : undefined;
    link.href = buildRecordingHlrecUrl(hlrecPath, req.defaultFilename);
  } else {
    link.href = buildExportCsvUrl(
      (req.params ?? { zoh: false, relativeTime: true }) as unknown as CsvExportParams,
      req.defaultFilename,
    );
  }
  document.body.append(link);
  link.click();
  link.remove();
  return { ok: true, message: `Downloaded ${req.defaultFilename}` };
}

export class ExportUiController {
  constructor(
    private readonly state: TimelineState,
    private readonly callbacks: {
      zoomToRange: (tA: number, tB: number) => void;
      setMeasureText: (text: string) => void;
    },
  ) {
    this.wireDom();
  }

  closePopover(): void {
    const pop = document.getElementById("export-popover");
    if (pop) pop.hidden = true;
  }

  openPopover(preferredRange?: ExportRangeMode): void {
    const pop = document.getElementById("export-popover");
    const btn = document.getElementById("export");
    if (!pop || !btn) return;

    const presetPop = document.getElementById("preset-popover");
    if (presetPop) presetPop.hidden = true;
    const eventsPop = document.getElementById("events-popover");
    if (eventsPop) eventsPop.hidden = true;

    const rect = btn.getBoundingClientRect();
    pop.style.left = `${Math.max(8, Math.min(rect.right - 340, window.innerWidth - 350))}px`;
    pop.style.top = `${rect.bottom + 4}px`;
    pop.style.right = "auto";
    pop.style.bottom = "auto";
    pop.hidden = false;

    const hasCursors = this.state.cursorA !== null && this.state.cursorB !== null;
    const hasRecording = this.state.lastRecording !== null;
    const chosen: ExportRangeMode =
      preferredRange === "recording" && hasRecording
        ? "recording"
        : preferredRange === "cursors" && hasCursors
          ? "cursors"
          : preferredRange && preferredRange !== "recording" && preferredRange !== "cursors"
            ? preferredRange
            : defaultExportRangeMode(this.state);

    const rCursors = document.getElementById("exp-range-cursors") as HTMLInputElement | null;
    const rRecording = document.getElementById("exp-range-recording") as HTMLInputElement | null;
    const cursorMeta = document.getElementById("exp-cursors-meta");
    const visibleMeta = document.getElementById("exp-visible-meta");
    const recMeta = document.getElementById("exp-recording-meta");
    const visibleOnly = document.getElementById("exp-visible-only") as HTMLInputElement | null;
    const alignSparse = document.getElementById("exp-align-sparse") as HTMLInputElement | null;
    const alignZoh = document.getElementById("exp-align-zoh") as HTMLInputElement | null;
    const relTime = document.getElementById("exp-relative-time") as HTMLInputElement | null;
    const fileInput = document.getElementById("exp-filename") as HTMLInputElement | null;

    if (rCursors) rCursors.disabled = !hasCursors;
    if (rRecording) rRecording.disabled = !hasRecording;

    if (cursorMeta) {
      if (hasCursors && this.state.cursorA !== null && this.state.cursorB !== null) {
        const dt = Math.abs(this.state.cursorB - this.state.cursorA);
        cursorMeta.textContent = `(${dt.toFixed(3)} s)`;
      } else {
        cursorMeta.textContent = "(place A & B first)";
      }
    }
    if (visibleMeta) {
      visibleMeta.textContent = `(${spanSeconds(this.state.division).toFixed(3)} s)`;
    }
    if (recMeta) {
      recMeta.textContent = this.state.lastRecording
        ? `(${this.state.lastRecording.samples.toLocaleString()} pts)`
        : "(none yet)";
    }

    const radio = document.querySelector<HTMLInputElement>(
      `input[name="exp-range"][value="${chosen}"]`,
    );
    if (radio) radio.checked = true;

    const visibleCount = this.state.series.filter((s) => isSeriesVisible(this.state, s)).length;
    if (visibleOnly) visibleOnly.checked = visibleCount > 0;
    const preferZoh = chosen === "recording" || visibleCount > 1;
    if (alignZoh && alignSparse) {
      alignZoh.checked = preferZoh;
      alignSparse.checked = !preferZoh;
    }
    if (relTime) relTime.checked = true;
    if (fileInput) {
      fileInput.value = defaultExportFilename(this.state.activePresetName, "csv");
      fileInput.focus();
      fileInput.select();
    }
  }

  showRecordingBanner(): void {
    const rec = this.state.lastRecording;
    const banner = document.getElementById("record-banner");
    const summary = document.getElementById("record-banner-summary");
    const zoomBtn = document.getElementById("record-banner-zoom") as HTMLButtonElement | null;
    if (!rec || !banner || !summary) return;

    const durSec =
      rec.startedAt !== null && rec.endedAt !== null
        ? Math.max(0, rec.endedAt - rec.startedAt)
        : null;
    const durPart = durSec !== null ? `${durSec.toFixed(2)} s` : "recording";
    const dropPart = rec.dropped > 0 ? ` · ${rec.dropped} dropped` : "";
    summary.textContent = `Recorded ${durPart} · ${rec.samples.toLocaleString()} samples${dropPart}`;
    if (zoomBtn) {
      zoomBtn.hidden = rec.startedAt === null || rec.endedAt === null;
    }
    banner.hidden = false;
  }

  hideRecordingBanner(): void {
    const banner = document.getElementById("record-banner");
    if (banner) banner.hidden = true;
  }

  private async submitExport(): Promise<void> {
    const rangeInput = document.querySelector<HTMLInputElement>('input[name="exp-range"]:checked');
    const mode = (rangeInput?.value as ExportRangeMode | undefined) ?? "visible";
    const visibleOnly =
      (document.getElementById("exp-visible-only") as HTMLInputElement | null)?.checked ?? true;
    const alignZoh =
      (document.getElementById("exp-align-zoh") as HTMLInputElement | null)?.checked ?? false;
    const relativeTime =
      (document.getElementById("exp-relative-time") as HTMLInputElement | null)?.checked ?? true;
    const rawName =
      (document.getElementById("exp-filename") as HTMLInputElement | null)?.value.trim() ||
      defaultExportFilename(this.state.activePresetName, "csv");
    const filename = rawName.endsWith(".csv") ? rawName : `${rawName}.csv`;

    const selectedSeries = visibleOnly
      ? this.state.series.filter((s) => isSeriesVisible(this.state, s))
      : this.state.series;

    const params: Record<string, unknown> = {
      zoh: alignZoh,
      relativeTime,
    };

    if (mode === "recording") {
      if (!this.state.lastRecording) {
        this.callbacks.setMeasureText("No recording available to export.");
        return;
      }
      params.fromRecording = true;
      params.hlrecPath = this.state.lastRecording.path;
      if (visibleOnly && selectedSeries.length > 0 && selectedSeries.length < this.state.series.length) {
        params.seriesNames = selectedSeries.map((s) => s.name);
      }
    } else {
      if (selectedSeries.length === 0) {
        this.callbacks.setMeasureText("No signals selected for CSV export.");
        return;
      }
      params.seriesIds = selectedSeries.map((s) => s.seriesId);
      const bounds = resolveTimeBoundsNs(mode, this.state, nowSeconds(this.state));
      if (bounds.t0 !== undefined) params.t0 = bounds.t0;
      if (bounds.t1 !== undefined) params.t1 = bounds.t1;
    }

    this.closePopover();
    this.callbacks.setMeasureText(`Preparing CSV export (${filename})…`);
    const res = await saveExportViaHostOrBrowser({
      kind: "csv",
      defaultFilename: filename,
      params,
    });
    if (res.cancelled) {
      this.callbacks.setMeasureText("CSV export cancelled.");
    } else if (res.ok) {
      this.callbacks.setMeasureText(res.message ?? `Exported ${filename}`);
    } else {
      this.callbacks.setMeasureText(`CSV export failed: ${res.message ?? "unknown error"}`);
    }
  }

  private wireDom(): void {
    const pop = document.getElementById("export-popover");
    const btn = document.getElementById("export");
    const form = document.getElementById("export-form") as HTMLFormElement | null;
    const closeBtn = document.getElementById("export-close");

    btn?.addEventListener("click", () => {
      if (pop && !pop.hidden) {
        this.closePopover();
      } else {
        this.openPopover();
      }
    });

    closeBtn?.addEventListener("click", () => this.closePopover());

    form?.addEventListener("submit", (event) => {
      event.preventDefault();
      void this.submitExport();
    });

    document.getElementById("record-banner-csv")?.addEventListener("click", () => {
      this.openPopover("recording");
    });

    document.getElementById("record-banner-zoom")?.addEventListener("click", () => {
      const rec = this.state.lastRecording;
      if (rec && rec.startedAt !== null && rec.endedAt !== null) {
        this.callbacks.zoomToRange(rec.startedAt, Math.max(rec.startedAt + 0.01, rec.endedAt));
      }
    });

    document.getElementById("record-banner-hlrec")?.addEventListener("click", () => {
      const rec = this.state.lastRecording;
      if (!rec) return;
      const filename = defaultExportFilename(this.state.activePresetName, "hlrec");
      void saveExportViaHostOrBrowser({
        kind: "hlrec",
        defaultFilename: filename,
        params: { hlrecPath: rec.path },
      }).then((res) => {
        if (res.cancelled) {
          this.callbacks.setMeasureText("Recording save cancelled.");
        } else {
          this.callbacks.setMeasureText(res.message ?? `Saved ${filename}`);
        }
      });
    });

    document.getElementById("record-banner-close")?.addEventListener("click", () => {
      this.hideRecordingBanner();
    });

    window.addEventListener("mousedown", (event) => {
      if (!pop || pop.hidden) return;
      const target = event.target as Node;
      if (!pop.contains(target) && btn && !btn.contains(target)) {
        this.closePopover();
      }
    });
  }
}
