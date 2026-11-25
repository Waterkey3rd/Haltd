import "./liveWatchApp.css";
import {
  ALL_OPTIONAL_COLUMNS,
  DEFAULT_COLUMN_WIDTHS,
  type ColumnWidths,
  type HostToWebviewMessage,
  type SymbolCompletionItem,
  type TargetRunState,
  type WatchColumnKey,
  type WatchFormat,
  type WatchRowNode,
  type WatchpointAccess,
  type WebviewToHostMessage,
} from "../shared/liveWatchProtocol";

declare function acquireVsCodeApi(): {
  postMessage(message: WebviewToHostMessage): void;
  getState<T>(): T | undefined;
  setState<T>(state: T): void;
};

const vscode = acquireVsCodeApi();

const ROW_HEIGHT_PX = 24;
const VIEWPORT_OVERSCAN_PX = 96;

const COLUMN_LABELS: Record<WatchColumnKey, string> = {
  type: "Type",
  address: "Address",
  rate: "Rate",
};

const FORMAT_OPTIONS: Array<{ key: WatchFormat; short: string; label: string }> = [
  { key: "default", short: "auto", label: "Default" },
  { key: "dec", short: "dec", label: "Signed Decimal" },
  { key: "uint", short: "uint", label: "Unsigned Decimal" },
  { key: "hex", short: "hex", label: "Hexadecimal" },
  { key: "bin", short: "bin", label: "Binary (0b)" },
  { key: "float", short: "flt", label: "IEEE-754 Float" },
  { key: "char", short: "chr", label: "ASCII Char" },
];

const FORMAT_SHORT_BY_KEY = new Map<WatchFormat, string>(
  FORMAT_OPTIONS.map((opt) => [opt.key, opt.short]),
);

const RATE_OPTIONS = [
  { value: 0, label: "Stop" },
  { value: 1, label: "1 Hz" },
  { value: 2, label: "2 Hz" },
  { value: 5, label: "5 Hz" },
  { value: 10, label: "10 Hz" },
  { value: 20, label: "20 Hz" },
];

// Reusable scratch buffer to avoid per-cell allocations during IEEE-754 formatting.
const SCALAR_BUF = new ArrayBuffer(8);
const SCALAR_VIEW = new DataView(SCALAR_BUF);

function svgChevron(expanded: boolean): SVGSVGElement {
  const svg = document.createElementNS("http://www.w3.org/2000/svg", "svg");
  svg.setAttribute("width", "12");
  svg.setAttribute("height", "12");
  svg.setAttribute("viewBox", "0 0 16 16");
  const path = document.createElementNS("http://www.w3.org/2000/svg", "path");
  path.setAttribute("fill", "currentColor");
  path.setAttribute(
    "d",
    expanded
      ? "M3.2 5.7a.75.75 0 0 1 1.06-.05L8 9.09l3.74-3.44a.75.75 0 1 1 1.02 1.1l-4.25 3.9a.75.75 0 0 1-1.02 0l-4.25-3.9a.75.75 0 0 1-.04-1.05z"
      : "M5.7 3.2a.75.75 0 0 1 1.05.04l3.9 4.25a.75.75 0 0 1 0 1.02l-3.9 4.25a.75.75 0 1 1-1.1-1.02L9.09 8 5.65 4.26A.75.75 0 0 1 5.7 3.2z",
  );
  svg.append(path);
  return svg;
}

function svgPlus(): SVGSVGElement {
  const svg = document.createElementNS("http://www.w3.org/2000/svg", "svg");
  svg.setAttribute("width", "12");
  svg.setAttribute("height", "12");
  svg.setAttribute("viewBox", "0 0 16 16");
  const path = document.createElementNS("http://www.w3.org/2000/svg", "path");
  path.setAttribute("fill", "currentColor");
  path.setAttribute(
    "d",
    "M8 2.5a.6.6 0 0 1 .6.6v4.3h4.3a.6.6 0 0 1 0 1.2H8.6v4.3a.6.6 0 0 1-1.2 0V8.6H3.1a.6.6 0 0 1 0-1.2h4.3V3.1a.6.6 0 0 1 .6-.6z",
  );
  svg.append(path);
  return svg;
}

function svgWaveform(): SVGSVGElement {
  const svg = document.createElementNS("http://www.w3.org/2000/svg", "svg");
  svg.setAttribute("width", "12");
  svg.setAttribute("height", "12");
  svg.setAttribute("viewBox", "0 0 16 16");
  const path = document.createElementNS("http://www.w3.org/2000/svg", "path");
  path.setAttribute("fill", "none");
  path.setAttribute("stroke", "currentColor");
  path.setAttribute("stroke-width", "1.4");
  path.setAttribute("stroke-linecap", "round");
  path.setAttribute("stroke-linejoin", "round");
  path.setAttribute("d", "M1.5 9.5H4L6 3.5L9.2 12.5L11.5 7.5H14.5");
  svg.append(path);
  return svg;
}

function svgMemory(): SVGSVGElement {
  const svg = document.createElementNS("http://www.w3.org/2000/svg", "svg");
  svg.setAttribute("width", "12");
  svg.setAttribute("height", "12");
  svg.setAttribute("viewBox", "0 0 16 16");
  const path = document.createElementNS("http://www.w3.org/2000/svg", "path");
  path.setAttribute("fill", "currentColor");
  path.setAttribute(
    "d",
    "M3.5 3h9A1.5 1.5 0 0 1 14 4.5v7a1.5 1.5 0 0 1-1.5 1.5h-9A1.5 1.5 0 0 1 2 11.5v-7A1.5 1.5 0 0 1 3.5 3zm0 1.2a.3.3 0 0 0-.3.3v7a.3.3 0 0 0 .3.3h9a.3.3 0 0 0 .3-.3v-7a.3.3 0 0 0-.3-.3h-9zM5 6h2v1.4H5V6zm4 0h2v1.4H9V6zm-4 3h2v1.4H5V9zm4 0h2v1.4H9V9z",
  );
  svg.append(path);
  return svg;
}

function svgCopy(): SVGSVGElement {
  const svg = document.createElementNS("http://www.w3.org/2000/svg", "svg");
  svg.setAttribute("width", "12");
  svg.setAttribute("height", "12");
  svg.setAttribute("viewBox", "0 0 16 16");
  const path = document.createElementNS("http://www.w3.org/2000/svg", "path");
  path.setAttribute("fill", "currentColor");
  path.setAttribute(
    "d",
    "M5 2.5A1.5 1.5 0 0 1 6.5 1h6A1.5 1.5 0 0 1 14 2.5v7A1.5 1.5 0 0 1 12.5 11H11v1.5A1.5 1.5 0 0 1 9.5 14h-6A1.5 1.5 0 0 1 2 12.5v-7A1.5 1.5 0 0 1 3.5 4H5V2.5zm1.2 1.5H9.5A1.5 1.5 0 0 1 11 5.5v4.3h1.5a.3.3 0 0 0 .3-.3v-7a.3.3 0 0 0-.3-.3h-6a.3.3 0 0 0-.3.3V4zM3.5 5.2a.3.3 0 0 0-.3.3v7a.3.3 0 0 0 .3.3h6a.3.3 0 0 0 .3-.3v-7a.3.3 0 0 0-.3-.3h-6z",
  );
  svg.append(path);
  return svg;
}

const CHEVRON_DOWN_SVG = svgChevron(true);
const CHEVRON_RIGHT_SVG = svgChevron(false);

const TOGGLE_BTN_DOWN_TPL = (() => {
  const btn = document.createElement("button");
  btn.type = "button";
  btn.className = "lw-toggle-btn";
  btn.dataset.action = "toggle";
  btn.append(CHEVRON_DOWN_SVG.cloneNode(true));
  return btn;
})();

const TOGGLE_BTN_RIGHT_TPL = (() => {
  const btn = document.createElement("button");
  btn.type = "button";
  btn.className = "lw-toggle-btn";
  btn.dataset.action = "toggle";
  btn.append(CHEVRON_RIGHT_SVG.cloneNode(true));
  return btn;
})();

const TOGGLE_SPACER_TPL = (() => {
  const sp = document.createElement("span");
  sp.className = "lw-toggle-spacer";
  return sp;
})();

const WP_DOT_TPL = (() => {
  const dot = document.createElement("span");
  dot.className = "lw-wp-dot";
  return dot;
})();

const WAVE_HOVER_TPL = (() => {
  const wrap = document.createElement("span");
  wrap.className = "lw-hover-actions";
  const btn = document.createElement("button");
  btn.type = "button";
  btn.className = "lw-mini-icon-btn";
  btn.dataset.action = "plot";
  btn.title = "Add to Timeline (Ctrl+G)";
  btn.append(svgWaveform());
  wrap.append(btn);
  return wrap;
})();

const ADDR_HOVER_TPL = (() => {
  const wrap = document.createElement("span");
  wrap.className = "lw-hover-actions";
  const mem = document.createElement("button");
  mem.type = "button";
  mem.className = "lw-mini-icon-btn";
  mem.dataset.action = "memory";
  mem.title = "View in Memory";
  mem.append(svgMemory());
  const copy = document.createElement("button");
  copy.type = "button";
  copy.className = "lw-mini-icon-btn";
  copy.dataset.action = "copy-addr";
  copy.title = "Copy address";
  copy.append(svgCopy());
  wrap.append(mem, copy);
  return wrap;
})();

const RATE_SELECT_TPL = (() => {
  const sel = document.createElement("select");
  sel.className = "lw-rate-select";
  for (const opt of RATE_OPTIONS) {
    sel.append(new Option(opt.label, String(opt.value)));
  }
  return sel;
})();

function parseBitWidth(row: WatchRowNode): number {
  if (row.bitRange) {
    const m = row.bitRange.match(/^\[(\d+)(?::(\d+))?\]$/);
    if (m) {
      const hi = Number(m[1]);
      const lo = m[2] !== undefined ? Number(m[2]) : hi;
      return Math.max(1, hi - lo + 1);
    }
  }
  return Math.max(8, Math.min(64, (row.size ?? 4) * 8));
}

function groupBinary(bitsStr: string): string {
  const pad = Math.ceil(bitsStr.length / 4) * 4;
  const padded = bitsStr.padStart(pad, "0");
  const parts: string[] = [];
  for (let i = 0; i < padded.length; i += 4) {
    parts.push(padded.slice(i, i + 4));
  }
  return `0b${parts.join("_")}`;
}

function formatScalarValue(row: WatchRowNode): string {
  if (row.error) {
    return row.error;
  }
  if (row.format === "default" || !row.rawHex) {
    return row.value;
  }
  let raw: bigint;
  try {
    raw = BigInt(row.rawHex);
  } catch {
    return row.value;
  }
  const bits = parseBitWidth(row);
  const mask = bits >= 64 ? 0xffff_ffff_ffff_ffffn : (1n << BigInt(bits)) - 1n;
  const uval = raw & mask;
  const signBit = 1n << BigInt(bits - 1);

  switch (row.format) {
    case "uint":
      return uval.toString(10);
    case "dec": {
      const sval = uval >= signBit ? uval - (1n << BigInt(bits)) : uval;
      return sval.toString(10);
    }
    case "hex": {
      const digits = Math.max(1, Math.ceil(bits / 4));
      return `0x${uval.toString(16).padStart(digits, "0")}`;
    }
    case "bin":
      return groupBinary(uval.toString(2).padStart(bits, "0"));
    case "float": {
      if (bits <= 32) {
        SCALAR_VIEW.setUint32(0, Number(uval & 0xffff_ffffn), true);
        const f = SCALAR_VIEW.getFloat32(0, true);
        return Number.isFinite(f) ? String(Number(f.toPrecision(7))) : String(f);
      }
      SCALAR_VIEW.setBigUint64(0, uval, true);
      const d = SCALAR_VIEW.getFloat64(0, true);
      return String(d);
    }
    case "char": {
      const byteCount = Math.max(1, Math.min(8, Math.ceil(bits / 8)));
      const chars: string[] = [];
      for (let i = 0; i < byteCount; i++) {
        const b = Number((uval >> BigInt(i * 8)) & 0xffn);
        if (b === 0) break;
        if (b === 10) chars.push("\\n");
        else if (b === 13) chars.push("\\r");
        else if (b === 9) chars.push("\\t");
        else if (b >= 0x20 && b <= 0x7e) chars.push(String.fromCharCode(b));
        else chars.push(`\\x${b.toString(16).padStart(2, "0")}`);
      }
      return byteCount === 1 ? `'${chars.join("") || "\\0"}'` : `"${chars.join("")}"`;
    }
    default:
      return row.value;
  }
}

function formatTooltip(row: WatchRowNode): string {
  const lines = [row.expr];
  if (row.type) lines.push(`Type: ${row.type}`);
  if (row.address) lines.push(`Address: ${row.address}${row.bitRange ?? ""}`);
  if (row.rawHex) lines.push(`Raw Hex: ${row.rawHex}`);
  if (row.prevValue !== null && (row.lastChangedAtMs !== null || row.changedOnStop)) {
    const ago =
      row.lastChangedAtMs !== null
        ? ` (${((Date.now() - row.lastChangedAtMs) / 1000).toFixed(1)}s ago)`
        : "";
    lines.push(
      `Prev: ${row.prevValue}${row.prevRawHex ? ` (${row.prevRawHex})` : ""} -> Now: ${row.value}${ago}`,
    );
  }
  if (row.error) lines.push(`Error: ${row.error}`);
  return lines.join("\n");
}

interface RowDom {
  el: HTMLElement;
  nameCell: HTMLElement;
  toggleSlot: HTMLElement;
  wpDot: HTMLElement | null;
  nameSpan: HTMLElement;
  waveActs: HTMLElement | null;
  valCell: HTMLElement;
  valText: HTMLElement;
  fmtBtn: HTMLButtonElement;
  typeSpan: HTMLElement | null;
  addrCell: HTMLElement | null;
  addrSpan: HTMLElement | null;
  addrHover: HTMLElement | null;
  rateSelect: HTMLSelectElement | null;
  toggleAlt: boolean;
  lastDepth: number;
  lastParentIsNull: boolean;
  lastExpandable: boolean;
  lastExpanded: boolean;
  lastWatchpoint: WatchpointAccess | null;
  lastName: string;
  lastPlottable: boolean;
  lastWritable: boolean;
  lastError: string | null;
  lastText: string;
  lastHasRaw: boolean;
  lastFmt: WatchFormat;
  lastTypeText: string;
  lastAddr: string | null;
  lastAddrText: string;
  lastRate: number;
}

class LiveWatchApp {
  private rows: WatchRowNode[] = [];
  private byId = new Map<number, WatchRowNode>();
  private columns: WatchColumnKey[] = [...ALL_OPTIONAL_COLUMNS];
  private widths: ColumnWidths = { ...DEFAULT_COLUMN_WIDTHS };
  private freshnessMs = 2500;
  private runState: TargetRunState = "disconnected";
  private connected = false;

  private selectedIds = new Set<number>();
  private focusedId: number | null = null;
  private renderedSelectedIds = new Set<number>();
  private renderedFocusedId: number | null = null;
  private filterText = "";
  private filterVisible = false;
  private editing: { id: number; field: "value" | "expr" } | null = null;

  private compRequestId = 1;
  private completions: SymbolCompletionItem[] = [];
  private compActiveIndex = -1;
  private compTimer: number | undefined;
  private errorMessage: string | null = null;
  private errorTimer: number | undefined;
  private dragRootId: number | null = null;
  private dragOverRowEl: HTMLElement | null = null;

  private readonly rowDoms = new Map<number, RowDom>();
  private renderedColumnsKey = "";
  private visibleOrder: WatchRowNode[] = [];
  private readonly visibleIndexById = new Map<number, number>();
  private contextMenu: HTMLElement | null = null;

  private rafPending = false;
  private statusDirty = false;
  private gridColsDirty = false;
  private bodyDirty = false;
  private freshnessAllDirty = false;
  private pendingRevealId: number | null = null;
  private readonly dirtyRowIds = new Set<number>();
  private readonly freshRowIds = new Set<number>();
  private lastBannerKey = "";
  private lastStale = false;

  private statusBanner!: HTMLElement;
  private filterBar!: HTMLElement;
  private filterInput!: HTMLInputElement;
  private tableWrap!: HTMLElement;
  private headerEl!: HTMLElement;
  private bodyEl!: HTMLElement;
  private addInput!: HTMLInputElement;
  private compPopup!: HTMLElement;

  constructor(root: HTMLElement) {
    this.buildShell(root);
    this.wireGlobalEvents();
    vscode.postMessage({ type: "ready" });
  }

  private buildShell(root: HTMLElement): void {
    this.statusBanner = document.createElement("div");
    this.statusBanner.className = "lw-status-banner";
    this.statusBanner.hidden = true;

    this.filterBar = document.createElement("div");
    this.filterBar.className = "lw-filter-bar";
    this.filterBar.hidden = true;
    this.filterInput = document.createElement("input");
    this.filterInput.className = "lw-filter-input";
    this.filterInput.type = "search";
    this.filterInput.placeholder = "Filter expressions or types (Esc to close)…";
    this.filterInput.addEventListener("input", () => {
      this.filterText = this.filterInput.value;
      this.bodyDirty = true;
      this.scheduleRender();
    });
    this.filterInput.addEventListener("keydown", (e) => {
      if (e.key === "Escape") {
        e.preventDefault();
        this.filterText = "";
        this.filterInput.value = "";
        this.filterVisible = false;
        this.filterBar.hidden = true;
        this.bodyDirty = true;
        this.scheduleRender();
        this.tableWrap.focus();
      }
    });
    this.filterBar.append(this.filterInput);

    this.tableWrap = document.createElement("div");
    this.tableWrap.className = "lw-table-wrap";
    this.tableWrap.tabIndex = 0;

    this.headerEl = document.createElement("div");
    this.headerEl.className = "lw-header";

    this.bodyEl = document.createElement("div");
    this.bodyEl.className = "lw-body";

    const addRow = document.createElement("div");
    addRow.className = "lw-add-row";
    const addCell = document.createElement("div");
    addCell.className = "lw-add-cell";
    const plus = svgPlus();
    plus.classList.add("lw-add-icon");
    this.addInput = document.createElement("input");
    this.addInput.className = "lw-add-input";
    this.addInput.type = "text";
    this.addInput.placeholder = "Add expression (e.g. g_motor.speed, *g_ptr, adc * 3.3 / 4096)…";
    this.addInput.autocomplete = "off";
    this.addInput.spellcheck = false;
    addCell.append(plus, this.addInput);
    addRow.append(addCell);

    this.compPopup = document.createElement("div");
    this.compPopup.className = "lw-completions";
    this.compPopup.hidden = true;

    this.tableWrap.append(this.headerEl, this.bodyEl, addRow);
    root.append(this.statusBanner, this.filterBar, this.tableWrap, this.compPopup);

    this.wireAddInput();
    this.wireBodyDelegation();
    this.applyGridColumns();
    this.renderHeader();
  }

  private rowFromTarget(target: EventTarget | null): { rowEl: HTMLElement; row: WatchRowNode } | null {
    if (!(target instanceof Element)) return null;
    const rowEl = target.closest<HTMLElement>(".lw-row");
    const rawId = rowEl?.dataset.id;
    if (!rowEl || !rawId) return null;
    const row = this.byId.get(Number(rawId));
    return row ? { rowEl, row } : null;
  }

  private wireBodyDelegation(): void {
    this.bodyEl.addEventListener("click", (e) => {
      const target = e.target as Element | null;
      if (target?.closest(".lw-rate-select, .lw-inline-input")) {
        return;
      }
      const hit = this.rowFromTarget(target);
      if (!hit) return;
      const { row } = hit;

      const actEl = target?.closest<HTMLElement>("[data-action]");
      if (actEl) {
        e.stopPropagation();
        switch (actEl.dataset.action) {
          case "toggle":
            vscode.postMessage({ type: row.expanded ? "collapse" : "expand", id: row.id });
            break;
          case "plot":
            vscode.postMessage({ type: "addToTimeline", exprs: [row.expr] });
            break;
          case "format": {
            const idx = FORMAT_OPTIONS.findIndex((f) => f.key === row.format);
            const next = FORMAT_OPTIONS[(idx + 1) % FORMAT_OPTIONS.length].key;
            vscode.postMessage({ type: "setFormat", id: row.id, format: next });
            break;
          }
          case "memory":
            if (row.address) {
              vscode.postMessage({ type: "viewMemory", address: row.address, name: row.expr });
            }
            break;
          case "copy-addr":
            if (row.address) {
              vscode.postMessage({ type: "copy", text: row.address });
            }
            break;
        }
        return;
      }

      this.tableWrap.focus();
      if (e.ctrlKey || e.metaKey) {
        if (this.selectedIds.has(row.id)) this.selectedIds.delete(row.id);
        else this.selectedIds.add(row.id);
      } else {
        this.selectedIds = new Set([row.id]);
      }
      this.focusedId = row.id;
      this.updateSelectionStyles();
    });

    this.bodyEl.addEventListener("dblclick", (e) => {
      const target = e.target as Element | null;
      if (target?.closest(".lw-fmt-tag, [data-action], .lw-rate-select, .lw-inline-input")) {
        return;
      }
      const hit = this.rowFromTarget(target);
      if (!hit) return;
      const { row } = hit;

      if (target?.closest(".lw-cell-value")) {
        if (row.writable) {
          e.stopPropagation();
          this.startInlineEdit(row.id, "value");
        }
        return;
      }
      if (target?.closest(".lw-cell-name") && row.parent === null) {
        e.stopPropagation();
        this.startInlineEdit(row.id, "expr");
      }
    });

    this.bodyEl.addEventListener("contextmenu", (e) => {
      const hit = this.rowFromTarget(e.target);
      if (!hit) return;
      e.preventDefault();
      const { row } = hit;
      if (!this.selectedIds.has(row.id)) {
        this.selectedIds = new Set([row.id]);
        this.focusedId = row.id;
        this.updateSelectionStyles();
      }
      this.showRowContextMenu(row, e.clientX, e.clientY);
    });

    this.bodyEl.addEventListener("change", (e) => {
      const target = e.target;
      if (!(target instanceof HTMLSelectElement) || !target.classList.contains("lw-rate-select")) {
        return;
      }
      const hit = this.rowFromTarget(target);
      if (!hit) return;
      const hz = Number(target.value);
      vscode.postMessage({ type: "setRate", ids: [hit.row.id], rateHz: hz });
    });

    this.bodyEl.addEventListener("mouseover", (e) => {
      const target = e.target as Element | null;
      const cell = target?.closest<HTMLElement>(".lw-cell-name, .lw-cell-value");
      if (!cell) return;
      const hit = this.rowFromTarget(cell);
      if (!hit) return;
      const tip = formatTooltip(hit.row);
      if (cell.title !== tip) {
        cell.title = tip;
      }
    });

    this.bodyEl.addEventListener("dragstart", (e) => {
      const hit = this.rowFromTarget(e.target);
      if (!hit || hit.row.parent !== null) {
        e.preventDefault();
        return;
      }
      this.dragRootId = hit.row.id;
      e.dataTransfer?.setData("text/plain", String(hit.row.id));
    });

    this.bodyEl.addEventListener("dragover", (e) => {
      const hit = this.rowFromTarget(e.target);
      if (!hit || hit.row.parent !== null || this.dragRootId === null || this.dragRootId === hit.row.id) {
        return;
      }
      e.preventDefault();
      if (this.dragOverRowEl && this.dragOverRowEl !== hit.rowEl) {
        this.dragOverRowEl.classList.remove("is-drop-above");
      }
      this.dragOverRowEl = hit.rowEl;
      hit.rowEl.classList.add("is-drop-above");
    });

    this.bodyEl.addEventListener("dragleave", (e) => {
      if (!this.dragOverRowEl) return;
      const hit = this.rowFromTarget(e.target);
      if (hit && hit.rowEl === this.dragOverRowEl && !hit.rowEl.contains(e.relatedTarget as Node | null)) {
        hit.rowEl.classList.remove("is-drop-above");
        this.dragOverRowEl = null;
      }
    });

    this.bodyEl.addEventListener("drop", (e) => {
      if (this.dragOverRowEl) {
        this.dragOverRowEl.classList.remove("is-drop-above");
        this.dragOverRowEl = null;
      }
      const hit = this.rowFromTarget(e.target);
      if (!hit || hit.row.parent !== null || this.dragRootId === null || this.dragRootId === hit.row.id) {
        this.dragRootId = null;
        return;
      }
      e.preventDefault();
      const targetId = hit.row.id;
      const fromId = this.dragRootId;
      this.dragRootId = null;
      const roots = this.rows.filter((r) => r.parent === null).map((r) => r.id);
      const fromIdx = roots.indexOf(fromId);
      const toIdx = roots.indexOf(targetId);
      if (fromIdx < 0 || toIdx < 0) return;
      const [moved] = roots.splice(fromIdx, 1);
      roots.splice(toIdx, 0, moved);
      vscode.postMessage({ type: "reorder", rootIds: roots });
    });

    this.bodyEl.addEventListener("dragend", () => {
      this.dragRootId = null;
      if (this.dragOverRowEl) {
        this.dragOverRowEl.classList.remove("is-drop-above");
        this.dragOverRowEl = null;
      }
    });
  }

  private wireAddInput(): void {
    this.addInput.addEventListener("input", () => {
      const q = this.addInput.value.trim();
      if (this.compTimer !== undefined) {
        window.clearTimeout(this.compTimer);
      }
      if (!q || !this.connected) {
        this.hideCompletions();
        return;
      }
      this.compTimer = window.setTimeout(() => {
        const requestId = this.compRequestId++;
        vscode.postMessage({ type: "complete", requestId, query: q });
      }, 90);
    });

    this.addInput.addEventListener("keydown", (e) => {
      if (e.key === "ArrowDown" && !this.compPopup.hidden && this.completions.length > 0) {
        e.preventDefault();
        this.compActiveIndex = Math.min(this.completions.length - 1, this.compActiveIndex + 1);
        this.renderCompletions();
        return;
      }
      if (e.key === "ArrowUp" && !this.compPopup.hidden && this.completions.length > 0) {
        e.preventDefault();
        this.compActiveIndex = Math.max(0, this.compActiveIndex - 1);
        this.renderCompletions();
        return;
      }
      if (e.key === "Tab" && !this.compPopup.hidden && this.completions.length > 0) {
        e.preventDefault();
        const pick = this.completions[Math.max(0, this.compActiveIndex)];
        if (pick) {
          this.addInput.value = pick.name;
          this.hideCompletions();
        }
        return;
      }
      if (e.key === "Enter") {
        e.preventDefault();
        if (
          !this.compPopup.hidden &&
          this.compActiveIndex >= 0 &&
          this.completions[this.compActiveIndex]
        ) {
          this.addInput.value = this.completions[this.compActiveIndex].name;
          this.hideCompletions();
          return;
        }
        const expr = this.addInput.value.trim();
        if (expr) {
          vscode.postMessage({ type: "add", expr });
          this.addInput.value = "";
          this.hideCompletions();
        }
        return;
      }
      if (e.key === "Escape") {
        e.preventDefault();
        if (!this.compPopup.hidden) {
          this.hideCompletions();
        } else {
          this.addInput.blur();
          this.tableWrap.focus();
        }
      }
    });

    this.addInput.addEventListener("blur", () => {
      window.setTimeout(() => this.hideCompletions(), 150);
    });
  }

  private wireGlobalEvents(): void {
    window.addEventListener("message", (event) => {
      this.onHostMessage(event.data as HostToWebviewMessage);
    });

    window.addEventListener("mousedown", (e) => {
      if (this.contextMenu && !this.contextMenu.contains(e.target as Node)) {
        this.hideContextMenu();
      }
    });
    window.addEventListener("resize", () => {
      this.hideContextMenu();
      this.hideCompletions();
    });

    this.tableWrap.addEventListener("keydown", (e) => this.onTableKeyDown(e));
  }

  private onTableKeyDown(e: KeyboardEvent): void {
    const target = e.target as HTMLElement | null;
    if (
      target &&
      (target.tagName === "INPUT" || target.tagName === "SELECT" || target.tagName === "TEXTAREA")
    ) {
      return;
    }
    if (this.editing) {
      return;
    }

    const mod = e.ctrlKey || e.metaKey;
    const visibleRows = this.bodyDirty ? this.filteredRows() : this.visibleOrder;
    const curIdx =
      this.focusedId !== null ? visibleRows.findIndex((r) => r.id === this.focusedId) : -1;
    const focused = curIdx >= 0 ? visibleRows[curIdx] : undefined;

    if (mod && e.key.toLowerCase() === "f") {
      e.preventDefault();
      this.filterVisible = true;
      this.filterBar.hidden = false;
      this.filterInput.focus();
      this.filterInput.select();
      return;
    }

    if (!mod && (e.key === "/" || e.key === "+")) {
      e.preventDefault();
      this.addInput.focus();
      return;
    }

    if (e.key === "ArrowDown" || e.key === "ArrowUp") {
      if (visibleRows.length === 0) return;
      e.preventDefault();
      const delta = e.key === "ArrowDown" ? 1 : -1;
      const nextIdx =
        curIdx < 0 ? 0 : Math.max(0, Math.min(visibleRows.length - 1, curIdx + delta));
      const nextRow = visibleRows[nextIdx];
      if (!e.shiftKey) {
        this.selectedIds.clear();
      }
      this.selectedIds.add(nextRow.id);
      this.focusedId = nextRow.id;
      this.updateSelectionStyles();
      this.rowDoms.get(nextRow.id)?.el.scrollIntoView({ block: "nearest" });
      return;
    }

    if (e.key === "ArrowRight" && focused) {
      e.preventDefault();
      if (focused.expandable && !focused.expanded) {
        vscode.postMessage({ type: "expand", id: focused.id });
      }
      return;
    }

    if (e.key === "ArrowLeft" && focused) {
      e.preventDefault();
      if (focused.expandable && focused.expanded) {
        vscode.postMessage({ type: "collapse", id: focused.id });
      } else if (focused.parent !== null) {
        this.selectedIds = new Set([focused.parent]);
        this.focusedId = focused.parent;
        this.updateSelectionStyles();
        this.rowDoms.get(focused.parent)?.el.scrollIntoView({ block: "nearest" });
      }
      return;
    }

    if ((e.key === "F2" || e.key === "Enter") && focused) {
      e.preventDefault();
      if (focused.writable) {
        this.startInlineEdit(focused.id, "value");
      } else if (e.key === "Enter" && focused.expandable) {
        vscode.postMessage({ type: focused.expanded ? "collapse" : "expand", id: focused.id });
      } else if (e.key === "F2" && focused.parent === null) {
        this.startInlineEdit(focused.id, "expr");
      }
      return;
    }

    if ((e.key === "Delete" || e.key === "Backspace") && this.selectedIds.size > 0) {
      e.preventDefault();
      vscode.postMessage({ type: "remove", ids: [...this.selectedIds] });
      return;
    }

    if (mod && e.key.toLowerCase() === "c" && this.selectedIds.size > 0) {
      e.preventDefault();
      const text = this.rows
        .filter((r) => this.selectedIds.has(r.id))
        .map((r) => `${r.expr} = ${formatScalarValue(r)}`)
        .join("\n");
      if (text) {
        vscode.postMessage({ type: "copy", text });
      }
      return;
    }

    if (mod && e.key.toLowerCase() === "g" && this.selectedIds.size > 0) {
      e.preventDefault();
      const exprs = this.rows
        .filter((r) => this.selectedIds.has(r.id) && r.plottable)
        .map((r) => r.expr);
      if (exprs.length > 0) {
        vscode.postMessage({ type: "addToTimeline", exprs });
      }
      return;
    }

    if (mod && e.key.toLowerCase() === "r" && this.selectedIds.size > 0) {
      e.preventDefault();
      const firstId = [...this.selectedIds][0];
      this.rowDoms.get(firstId)?.rateSelect?.focus();
    }
  }

  private scheduleRender(): void {
    if (this.rafPending) {
      return;
    }
    this.rafPending = true;
    requestAnimationFrame(() => this.flushRender());
  }

  private flushRender(): void {
    this.rafPending = false;

    if (this.gridColsDirty) {
      this.gridColsDirty = false;
      this.applyGridColumns();
    }
    if (this.statusDirty) {
      this.statusDirty = false;
      this.renderStatusBanner();
    }

    const needViewport = this.bodyDirty || this.dirtyRowIds.size > 0 || this.freshnessAllDirty;
    const scrollTop = needViewport ? this.tableWrap.scrollTop : 0;
    const viewHeight = needViewport ? this.tableWrap.clientHeight : 0;

    if (this.bodyDirty) {
      this.bodyDirty = false;
      this.dirtyRowIds.clear();
      this.reconcileBody(scrollTop, viewHeight);
      this.freshRowIds.clear();
      this.freshnessAllDirty = false;
    } else {
      if (this.dirtyRowIds.size > 0) {
        for (const id of this.dirtyRowIds) {
          const row = this.byId.get(id);
          const dom = this.rowDoms.get(id);
          if (row && dom) {
            const triggerFresh = this.freshRowIds.has(id);
            const near = this.isRowNearViewport(id, scrollTop, viewHeight);
            this.updateRowDom(row, dom, triggerFresh, near);
          }
        }
        this.dirtyRowIds.clear();
        this.freshRowIds.clear();
      }
      if (this.freshnessAllDirty) {
        this.freshnessAllDirty = false;
        for (const row of this.visibleOrder) {
          const dom = this.rowDoms.get(row.id);
          if (dom) {
            const near = this.isRowNearViewport(row.id, scrollTop, viewHeight);
            this.updateRowFreshness(row, dom, near);
          }
        }
      }
    }

    if (this.pendingRevealId !== null) {
      const revealId = this.pendingRevealId;
      this.pendingRevealId = null;
      const dom = this.rowDoms.get(revealId);
      if (dom) {
        dom.el.scrollIntoView({ block: "nearest" });
        dom.el.classList.remove("is-reveal-flash");
        void dom.el.offsetWidth;
        dom.el.classList.add("is-reveal-flash");
      }
    }
  }

  private onHostMessage(msg: HostToWebviewMessage): void {
    switch (msg.type) {
      case "snapshot": {
        const colsChanged = msg.columns.join(",") !== this.columns.join(",");
        this.rows = msg.rows;
        this.byId = new Map(msg.rows.map((r) => [r.id, r]));
        this.columns = msg.columns;
        this.widths = msg.widths;
        this.freshnessMs = msg.freshnessMs;
        this.runState = msg.runState;
        this.connected = msg.connected;
        this.applyFreshnessVar();
        if (colsChanged) {
          this.renderHeader();
        }
        this.gridColsDirty = true;
        this.statusDirty = true;
        this.bodyDirty = true;
        this.scheduleRender();
        break;
      }
      case "patch":
        this.runState = msg.runState;
        this.connected = msg.connected;
        this.statusDirty = true;
        for (const u of msg.updates) {
          const row = this.byId.get(u.id);
          if (!row) continue;
          const valueChanged =
            row.value !== u.value || row.rawHex !== u.rawHex || row.error !== u.error;
          row.value = u.value;
          row.rawHex = u.rawHex;
          row.address = u.address;
          row.error = u.error;
          row.prevValue = u.prevValue;
          row.prevRawHex = u.prevRawHex;
          row.lastChangedAtMs = u.lastChangedAtMs;
          row.elapsedMs = u.elapsedMs;
          row.changedOnStop = u.changedOnStop;
          this.dirtyRowIds.add(u.id);
          if (valueChanged || u.changedOnStop) {
            this.freshRowIds.add(u.id);
          }
        }
        this.scheduleRender();
        break;
      case "metaPatch":
        this.runState = msg.runState;
        this.connected = msg.connected;
        this.statusDirty = true;
        for (const updated of msg.rows) {
          const existing = this.byId.get(updated.id);
          if (existing) {
            Object.assign(existing, updated);
            this.dirtyRowIds.add(updated.id);
            if (updated.changedOnStop) {
              this.freshRowIds.add(updated.id);
            }
          }
        }
        this.scheduleRender();
        break;
      case "state":
        this.runState = msg.runState;
        this.connected = msg.connected;
        this.freshnessMs = msg.freshnessMs;
        if (msg.runState === "running") {
          for (const row of this.rows) {
            row.changedOnStop = false;
          }
        }
        this.applyFreshnessVar();
        this.statusDirty = true;
        this.freshnessAllDirty = true;
        this.scheduleRender();
        break;
      case "columns":
        this.columns = msg.columns;
        this.widths = msg.widths;
        this.renderHeader();
        this.gridColsDirty = true;
        this.bodyDirty = true;
        this.scheduleRender();
        break;
      case "columnWidths":
        this.widths = msg.widths;
        this.gridColsDirty = true;
        this.scheduleRender();
        break;
      case "revealRow":
        this.selectedIds = new Set([msg.id]);
        this.focusedId = msg.id;
        this.updateSelectionStyles();
        this.pendingRevealId = msg.id;
        this.scheduleRender();
        break;
      case "focusAddInput":
        this.addInput.focus();
        this.addInput.select();
        break;
      case "completions":
        if (msg.requestId >= this.compRequestId - 1) {
          this.completions = msg.items;
          this.compActiveIndex = msg.items.length > 0 ? 0 : -1;
          this.renderCompletions();
        }
        break;
      case "actionError":
        this.errorMessage = msg.message;
        this.statusDirty = true;
        this.scheduleRender();
        if (this.errorTimer !== undefined) {
          window.clearTimeout(this.errorTimer);
        }
        this.errorTimer = window.setTimeout(() => {
          this.errorMessage = null;
          this.statusDirty = true;
          this.scheduleRender();
        }, 4000);
        break;
    }
  }

  private applyFreshnessVar(): void {
    document.documentElement.style.setProperty("--lw-fresh-ms", `${this.freshnessMs}ms`);
  }

  private activeColumnList(): Array<"name" | "value" | WatchColumnKey> {
    return ["name", "value", ...this.columns];
  }

  private applyGridColumns(): void {
    const parts = this.activeColumnList().map((col) => {
      const w = Math.max(48, this.widths[col] ?? DEFAULT_COLUMN_WIDTHS[col]);
      if (col === "value") {
        return `minmax(${w}px, 1.3fr)`;
      }
      if (col === "name") {
        return `minmax(120px, ${w}px)`;
      }
      return `${w}px`;
    });
    document.documentElement.style.setProperty("--lw-grid-cols", parts.join(" "));
  }

  private renderStatusBanner(): void {
    const mode = this.errorMessage
      ? `err:${this.errorMessage}`
      : !this.connected
        ? "disc"
        : "ok";
    if (this.lastStale !== !this.connected) {
      this.lastStale = !this.connected;
      this.bodyEl.classList.toggle("is-stale", this.lastStale);
    }
    if (mode === this.lastBannerKey) {
      return;
    }
    this.lastBannerKey = mode;

    if (this.errorMessage) {
      this.statusBanner.hidden = false;
      this.statusBanner.className = "lw-status-banner is-error";
      this.statusBanner.textContent = this.errorMessage;
      return;
    }
    this.statusBanner.className = "lw-status-banner";
    if (!this.connected) {
      this.statusBanner.hidden = false;
      this.statusBanner.textContent =
        "Disconnected · Showing cached watch list (start debugging to stream live values)";
    } else {
      this.statusBanner.hidden = true;
      this.statusBanner.textContent = "";
    }
  }

  private renderHeader(): void {
    const cols = this.activeColumnList();
    const labels: Record<"name" | "value" | WatchColumnKey, string> = {
      name: "Expression",
      value: "Value",
      type: "Type",
      address: "Address",
      rate: "Rate",
    };

    const children: HTMLElement[] = [];
    for (const col of cols) {
      const th = document.createElement("div");
      th.className = "lw-th";
      th.textContent = labels[col];
      th.title = "Right-click to toggle columns";
      th.addEventListener("contextmenu", (e) => {
        e.preventDefault();
        this.showHeaderContextMenu(e.clientX, e.clientY);
      });

      const resizer = document.createElement("div");
      resizer.className = "lw-col-resizer";
      resizer.addEventListener("mousedown", (e) => {
        e.preventDefault();
        e.stopPropagation();
        const startX = e.clientX;
        const startW = th.getBoundingClientRect().width;
        resizer.classList.add("is-dragging");
        const onMove = (me: MouseEvent) => {
          const nextW = Math.max(54, Math.min(520, Math.round(startW + (me.clientX - startX))));
          this.widths[col] = nextW;
          this.gridColsDirty = true;
          this.scheduleRender();
        };
        const onUp = () => {
          resizer.classList.remove("is-dragging");
          window.removeEventListener("mousemove", onMove);
          window.removeEventListener("mouseup", onUp);
          vscode.postMessage({ type: "setColumnWidths", widths: this.widths });
        };
        window.addEventListener("mousemove", onMove);
        window.addEventListener("mouseup", onUp);
      });

      th.append(resizer);
      children.push(th);
    }
    this.headerEl.replaceChildren(...children);
  }

  private filteredRows(): WatchRowNode[] {
    const needle = this.filterText.trim().toLowerCase();
    if (!needle) {
      return this.rows;
    }
    const rootCache = new Map<number, number>();
    const rootOf = (row: WatchRowNode): number => {
      const cached = rootCache.get(row.id);
      if (cached !== undefined) {
        return cached;
      }
      let cur: WatchRowNode | undefined = row;
      while (cur && cur.parent !== null) {
        const parentCached = rootCache.get(cur.parent);
        if (parentCached !== undefined) {
          rootCache.set(row.id, parentCached);
          return parentCached;
        }
        cur = this.byId.get(cur.parent);
      }
      const rootId = cur?.id ?? row.id;
      rootCache.set(row.id, rootId);
      return rootId;
    };
    const matchingRoots = new Set<number>();
    for (const row of this.rows) {
      const hay = `${row.name} ${row.expr} ${row.type} ${row.value}`.toLowerCase();
      if (hay.includes(needle)) {
        matchingRoots.add(rootOf(row));
      }
    }
    return this.rows.filter((r) => matchingRoots.has(rootOf(r)));
  }

  private isRowNearViewport(id: number, scrollTop: number, viewHeight: number): boolean {
    if (viewHeight <= 0) {
      return true;
    }
    const idx = this.visibleIndexById.get(id);
    if (idx === undefined) {
      return false;
    }
    const rowTop = idx * ROW_HEIGHT_PX;
    return (
      rowTop + ROW_HEIGHT_PX >= scrollTop - VIEWPORT_OVERSCAN_PX &&
      rowTop <= scrollTop + viewHeight + VIEWPORT_OVERSCAN_PX
    );
  }

  private reconcileBody(scrollTop: number, viewHeight: number): void {
    const colsKey = this.columns.join(",");
    if (this.renderedColumnsKey !== colsKey) {
      this.renderedColumnsKey = colsKey;
      this.rowDoms.clear();
      this.renderedSelectedIds.clear();
      this.renderedFocusedId = null;
      this.bodyEl.replaceChildren();
    }

    const visible = this.filteredRows();
    this.visibleOrder = visible;
    this.visibleIndexById.clear();
    for (let i = 0; i < visible.length; i++) {
      this.visibleIndexById.set(visible[i].id, i);
    }

    for (const [id, dom] of this.rowDoms.entries()) {
      if (!this.visibleIndexById.has(id)) {
        dom.el.remove();
        this.rowDoms.delete(id);
        this.renderedSelectedIds.delete(id);
        if (this.renderedFocusedId === id) {
          this.renderedFocusedId = null;
        }
      }
    }

    for (let i = 0; i < visible.length; i++) {
      const row = visible[i];
      const near = this.isRowNearViewport(row.id, scrollTop, viewHeight);
      let dom = this.rowDoms.get(row.id);
      if (!dom) {
        dom = this.createRowDom(row, near);
        this.rowDoms.set(row.id, dom);
      } else {
        const triggerFresh = this.freshRowIds.has(row.id) || this.freshnessAllDirty;
        this.updateRowDom(row, dom, triggerFresh, near, true);
      }
      if (this.bodyEl.children[i] !== dom.el) {
        this.bodyEl.insertBefore(dom.el, this.bodyEl.children[i] ?? null);
      }
    }

    this.updateSelectionStyles();
  }

  private createRowDom(row: WatchRowNode, nearViewport: boolean): RowDom {
    const el = document.createElement("div");
    el.className = "lw-row";
    el.dataset.id = String(row.id);
    const parentIsNull = row.parent === null;
    if (parentIsNull) {
      el.draggable = true;
    }

    // 1. Expression cell
    const nameCell = document.createElement("div");
    nameCell.className = "lw-cell lw-cell-name";
    nameCell.style.paddingLeft = `${7 + row.depth * 14}px`;

    const toggleSlot = (
      row.expandable
        ? row.expanded
          ? TOGGLE_BTN_DOWN_TPL
          : TOGGLE_BTN_RIGHT_TPL
        : TOGGLE_SPACER_TPL
    ).cloneNode(true) as HTMLElement;
    nameCell.append(toggleSlot);

    let wpDot: HTMLElement | null = null;
    if (row.watchpoint) {
      wpDot = WP_DOT_TPL.cloneNode(true) as HTMLElement;
      wpDot.title = `Hardware data breakpoint (${row.watchpoint})`;
      nameCell.append(wpDot);
    }

    const nameSpan = document.createElement("span");
    nameSpan.className = "lw-text-trunc lw-mono";
    nameSpan.textContent = row.name;
    nameCell.append(nameSpan);

    let waveActs: HTMLElement | null = null;
    if (row.plottable) {
      waveActs = WAVE_HOVER_TPL.cloneNode(true) as HTMLElement;
      nameCell.append(waveActs);
    }

    // 2. Value cell
    const valCell = document.createElement("div");
    valCell.className = "lw-cell lw-cell-value";
    if (row.writable) valCell.classList.add("is-writable");
    if (row.error) valCell.classList.add("is-error");

    const valText = document.createElement("span");
    valText.className = "lw-mono lw-val-text";
    const formattedText = formatScalarValue(row);
    valText.textContent = formattedText;

    const fmtBtn = document.createElement("button");
    fmtBtn.type = "button";
    fmtBtn.className = "lw-fmt-tag";
    fmtBtn.dataset.action = "format";
    fmtBtn.textContent = FORMAT_SHORT_BY_KEY.get(row.format) ?? "auto";
    fmtBtn.hidden = !row.rawHex;
    fmtBtn.title = "Click to cycle display format (or right-click row)";

    valCell.append(valText, fmtBtn);
    el.append(nameCell, valCell);

    let typeSpan: HTMLElement | null = null;
    let addrCell: HTMLElement | null = null;
    let addrSpan: HTMLElement | null = null;
    let addrHover: HTMLElement | null = null;
    let rateSelect: HTMLSelectElement | null = null;

    const typeText = row.type ? `${row.type}${row.bitRange ? ` ${row.bitRange}` : ""}` : "—";
    const addrText = row.address ? `${row.address}${row.bitRange ?? ""}` : "—";

    for (const col of this.columns) {
      if (col === "type") {
        const cell = document.createElement("div");
        cell.className = "lw-cell lw-cell-type";
        typeSpan = document.createElement("span");
        typeSpan.className = "lw-text-trunc lw-mono";
        typeSpan.textContent = typeText;
        typeSpan.title = typeText;
        cell.append(typeSpan);
        el.append(cell);
      } else if (col === "address") {
        addrCell = document.createElement("div");
        addrCell.className = "lw-cell lw-cell-addr";
        addrSpan = document.createElement("span");
        addrSpan.className = "lw-text-trunc lw-mono";
        addrSpan.textContent = addrText;
        addrSpan.title = addrText;
        addrCell.append(addrSpan);
        if (row.address) {
          addrHover = ADDR_HOVER_TPL.cloneNode(true) as HTMLElement;
          addrCell.append(addrHover);
        }
        el.append(addrCell);
      } else if (col === "rate") {
        const cell = document.createElement("div");
        cell.className = "lw-cell lw-cell-rate";
        rateSelect = RATE_SELECT_TPL.cloneNode(true) as HTMLSelectElement;
        rateSelect.value = String(row.rateHz);
        cell.append(rateSelect);
        el.append(cell);
      }
    }

    const dom: RowDom = {
      el,
      nameCell,
      toggleSlot,
      wpDot,
      nameSpan,
      waveActs,
      valCell,
      valText,
      fmtBtn,
      typeSpan,
      addrCell,
      addrSpan,
      addrHover,
      rateSelect,
      toggleAlt: false,
      lastDepth: row.depth,
      lastParentIsNull: parentIsNull,
      lastExpandable: row.expandable,
      lastExpanded: row.expanded,
      lastWatchpoint: row.watchpoint,
      lastName: row.name,
      lastPlottable: row.plottable,
      lastWritable: row.writable,
      lastError: row.error,
      lastText: formattedText,
      lastHasRaw: Boolean(row.rawHex),
      lastFmt: row.format,
      lastTypeText: typeText,
      lastAddr: row.address,
      lastAddrText: addrText,
      lastRate: row.rateHz,
    };
    this.updateRowFreshness(row, dom, nearViewport);
    return dom;
  }

  private updateRowDom(
    row: WatchRowNode,
    dom: RowDom,
    triggerFresh: boolean,
    nearViewport: boolean,
    syncStructure = false,
  ): void {
    const editingThis = this.editing?.id === row.id ? this.editing.field : null;

    if (syncStructure) {
      const parentIsNull = row.parent === null;
      if (parentIsNull !== dom.lastParentIsNull) {
        dom.lastParentIsNull = parentIsNull;
        dom.el.draggable = parentIsNull;
      }

      if (editingThis !== "expr") {
        if (row.depth !== dom.lastDepth) {
          dom.lastDepth = row.depth;
          dom.nameCell.style.paddingLeft = `${7 + row.depth * 14}px`;
        }
        if (row.expandable !== dom.lastExpandable || row.expanded !== dom.lastExpanded) {
          if (row.expandable && !dom.lastExpandable) {
            const nextSlot = (
              row.expanded ? TOGGLE_BTN_DOWN_TPL : TOGGLE_BTN_RIGHT_TPL
            ).cloneNode(true) as HTMLElement;
            dom.toggleSlot.replaceWith(nextSlot);
            dom.toggleSlot = nextSlot;
          } else if (!row.expandable && dom.lastExpandable) {
            const nextSlot = TOGGLE_SPACER_TPL.cloneNode(true) as HTMLElement;
            dom.toggleSlot.replaceWith(nextSlot);
            dom.toggleSlot = nextSlot;
          } else if (row.expandable && row.expanded !== dom.lastExpanded) {
            dom.toggleSlot.replaceChildren(
              (row.expanded ? CHEVRON_DOWN_SVG : CHEVRON_RIGHT_SVG).cloneNode(true),
            );
          }
          dom.lastExpandable = row.expandable;
          dom.lastExpanded = row.expanded;
        }
        if (row.name !== dom.lastName) {
          dom.lastName = row.name;
          dom.nameSpan.textContent = row.name;
        }
        if (row.plottable !== dom.lastPlottable) {
          dom.lastPlottable = row.plottable;
          if (row.plottable && !dom.waveActs) {
            dom.waveActs = WAVE_HOVER_TPL.cloneNode(true) as HTMLElement;
            dom.nameCell.append(dom.waveActs);
          } else if (!row.plottable && dom.waveActs) {
            dom.waveActs.remove();
            dom.waveActs = null;
          }
        }
      }
    }

    if (editingThis !== "expr" && row.watchpoint !== dom.lastWatchpoint) {
      dom.lastWatchpoint = row.watchpoint;
      if (row.watchpoint) {
        if (!dom.wpDot) {
          dom.wpDot = WP_DOT_TPL.cloneNode(true) as HTMLElement;
          dom.nameCell.insertBefore(dom.wpDot, dom.nameSpan);
        }
        dom.wpDot.title = `Hardware data breakpoint (${row.watchpoint})`;
      } else if (dom.wpDot) {
        dom.wpDot.remove();
        dom.wpDot = null;
      }
    }

    let valueMutated = false;
    if (editingThis !== "value") {
      if (row.writable !== dom.lastWritable) {
        dom.lastWritable = row.writable;
        dom.valCell.classList.toggle("is-writable", row.writable);
      }
      if (Boolean(row.error) !== Boolean(dom.lastError)) {
        dom.valCell.classList.toggle("is-error", Boolean(row.error));
      }
      if (row.error !== dom.lastError) {
        dom.lastError = row.error;
        valueMutated = true;
      }

      const nextText = formatScalarValue(row);
      if (nextText !== dom.lastText) {
        dom.lastText = nextText;
        dom.valText.textContent = nextText;
        valueMutated = true;
      }

      const hasRaw = Boolean(row.rawHex);
      if (hasRaw !== dom.lastHasRaw) {
        dom.lastHasRaw = hasRaw;
        dom.fmtBtn.hidden = !hasRaw;
      }
      if (row.format !== dom.lastFmt) {
        dom.lastFmt = row.format;
        dom.fmtBtn.textContent = FORMAT_SHORT_BY_KEY.get(row.format) ?? "auto";
      }

      if (triggerFresh || (syncStructure && valueMutated)) {
        this.updateRowFreshness(row, dom, nearViewport);
      }
    }

    if (dom.typeSpan) {
      const typeText = row.type ? `${row.type}${row.bitRange ? ` ${row.bitRange}` : ""}` : "—";
      if (typeText !== dom.lastTypeText) {
        dom.lastTypeText = typeText;
        dom.typeSpan.textContent = typeText;
        dom.typeSpan.title = typeText;
      }
    }

    if (dom.addrCell && dom.addrSpan) {
      const addrText = row.address ? `${row.address}${row.bitRange ?? ""}` : "—";
      if (addrText !== dom.lastAddrText) {
        dom.lastAddrText = addrText;
        dom.addrSpan.textContent = addrText;
        dom.addrSpan.title = addrText;
      }
      if (Boolean(row.address) !== Boolean(dom.lastAddr)) {
        if (row.address && !dom.addrHover) {
          dom.addrHover = ADDR_HOVER_TPL.cloneNode(true) as HTMLElement;
          dom.addrCell.append(dom.addrHover);
        } else if (!row.address && dom.addrHover) {
          dom.addrHover.remove();
          dom.addrHover = null;
        }
      }
      dom.lastAddr = row.address;
    }

    if (dom.rateSelect && row.rateHz !== dom.lastRate) {
      dom.lastRate = row.rateHz;
      dom.rateSelect.value = String(row.rateHz);
    }
  }

  private updateRowFreshness(row: WatchRowNode, dom: RowDom, nearViewport: boolean): void {
    const textEl = dom.valText;
    if (!this.connected || row.error) {
      textEl.classList.remove("is-fresh-a", "is-fresh-b", "is-stop-changed");
      if (textEl.style.animationDelay) textEl.style.animationDelay = "";
      return;
    }
    if (this.runState === "halted" && row.changedOnStop) {
      textEl.classList.remove("is-fresh-a", "is-fresh-b");
      if (textEl.style.animationDelay) textEl.style.animationDelay = "";
      textEl.classList.add("is-stop-changed");
      return;
    }
    textEl.classList.remove("is-stop-changed");

    if (!nearViewport || this.freshnessMs <= 0 || row.lastChangedAtMs === null) {
      textEl.classList.remove("is-fresh-a", "is-fresh-b");
      if (textEl.style.animationDelay) textEl.style.animationDelay = "";
      return;
    }
    const elapsed =
      row.elapsedMs !== null ? row.elapsedMs : Math.max(0, Date.now() - row.lastChangedAtMs);
    if (elapsed >= this.freshnessMs) {
      textEl.classList.remove("is-fresh-a", "is-fresh-b");
      if (textEl.style.animationDelay) textEl.style.animationDelay = "";
      return;
    }
    dom.toggleAlt = !dom.toggleAlt;
    textEl.classList.remove("is-fresh-a", "is-fresh-b");
    textEl.classList.add(dom.toggleAlt ? "is-fresh-a" : "is-fresh-b");
    textEl.style.animationDelay = `-${Math.round(elapsed)}ms`;
  }

  private updateSelectionStyles(): void {
    for (const prevId of this.renderedSelectedIds) {
      if (!this.selectedIds.has(prevId)) {
        this.rowDoms.get(prevId)?.el.classList.remove("is-selected");
      }
    }
    for (const nextId of this.selectedIds) {
      const dom = this.rowDoms.get(nextId);
      if (dom) {
        dom.el.classList.add("is-selected");
      }
    }
    this.renderedSelectedIds = new Set(this.selectedIds);

    if (this.renderedFocusedId !== null && this.renderedFocusedId !== this.focusedId) {
      this.rowDoms.get(this.renderedFocusedId)?.el.classList.remove("is-focused");
    }
    if (this.focusedId !== null) {
      this.rowDoms.get(this.focusedId)?.el.classList.add("is-focused");
    }
    this.renderedFocusedId = this.focusedId;
  }

  private startInlineEdit(id: number, field: "value" | "expr"): void {
    const row = this.byId.get(id);
    const dom = this.rowDoms.get(id);
    if (!row || !dom) return;
    if (field === "value" && !row.writable) return;
    if (field === "expr" && row.parent !== null) return;

    this.editing = { id, field };
    const hostCell = field === "value" ? dom.valCell : dom.nameCell;

    const prevChildren = [...hostCell.childNodes];
    const input = document.createElement("input");
    input.className = "lw-inline-input";
    input.type = "text";
    input.value = field === "value" ? formatScalarValue(row) : row.expr;
    hostCell.replaceChildren(input);
    input.focus();
    input.select();

    let finished = false;
    const cleanup = (commit: boolean) => {
      if (finished) return;
      finished = true;
      const nextText = input.value.trim();
      this.editing = null;
      hostCell.replaceChildren(...prevChildren);
      this.tableWrap.focus();
      if (commit && nextText) {
        if (field === "value") {
          vscode.postMessage({ type: "write", id, value: nextText });
        } else {
          vscode.postMessage({ type: "modify", id, expr: nextText });
        }
      } else {
        const latest = this.byId.get(id);
        const latestDom = this.rowDoms.get(id);
        if (latest && latestDom) {
          this.updateRowDom(latest, latestDom, false, true, true);
        }
      }
    };

    input.addEventListener("keydown", (e) => {
      e.stopPropagation();
      if (e.key === "Enter") {
        e.preventDefault();
        cleanup(true);
      } else if (e.key === "Escape") {
        e.preventDefault();
        cleanup(false);
      }
    });
    input.addEventListener("blur", () => cleanup(false));
  }

  private renderCompletions(): void {
    if (this.completions.length === 0) {
      this.hideCompletions();
      return;
    }
    const items = this.completions.map((item, idx) => {
      const row = document.createElement("div");
      row.className = "lw-comp-item" + (idx === this.compActiveIndex ? " is-active" : "");
      const name = document.createElement("span");
      name.className = "lw-comp-name";
      name.textContent = item.name;
      const meta = document.createElement("span");
      meta.className = "lw-comp-meta";
      meta.textContent = item.type ? `${item.type} · ${item.address}` : item.address;
      row.append(name, meta);
      row.addEventListener("mousedown", (e) => {
        e.preventDefault();
        vscode.postMessage({ type: "add", expr: item.name });
        this.addInput.value = "";
        this.hideCompletions();
      });
      return row;
    });
    this.compPopup.replaceChildren(...items);
    this.compPopup.hidden = false;

    const rect = this.addInput.getBoundingClientRect();
    const spaceBelow = window.innerHeight - rect.bottom;
    this.compPopup.style.left = `${Math.max(4, Math.min(rect.left, window.innerWidth - 300))}px`;
    if (spaceBelow >= 150) {
      this.compPopup.style.top = `${rect.bottom + 2}px`;
      this.compPopup.style.bottom = "";
    } else {
      this.compPopup.style.top = "";
      this.compPopup.style.bottom = `${window.innerHeight - rect.top + 2}px`;
    }
  }

  private hideCompletions(): void {
    this.completions = [];
    this.compActiveIndex = -1;
    this.compPopup.hidden = true;
  }

  private hideContextMenu(): void {
    if (this.contextMenu) {
      this.contextMenu.remove();
      this.contextMenu = null;
    }
  }

  private openMenuAt(x: number, y: number, builder: (menu: HTMLElement) => void): void {
    this.hideContextMenu();
    const menu = document.createElement("div");
    menu.className = "lw-context-menu";
    builder(menu);
    document.body.append(menu);
    this.contextMenu = menu;
    const rect = menu.getBoundingClientRect();
    const left = Math.min(x, Math.max(4, window.innerWidth - rect.width - 6));
    const top = Math.min(y, Math.max(4, window.innerHeight - rect.height - 6));
    menu.style.left = `${left}px`;
    menu.style.top = `${top}px`;
  }

  private addCtxItem(
    menu: HTMLElement,
    label: string,
    shortcut: string,
    onClick: () => void,
    disabled = false,
  ): void {
    const btn = document.createElement("button");
    btn.type = "button";
    btn.className = "lw-ctx-item";
    btn.disabled = disabled;
    const l = document.createElement("span");
    l.textContent = label;
    const s = document.createElement("span");
    s.className = "lw-ctx-shortcut";
    s.textContent = shortcut;
    btn.append(l, s);
    btn.addEventListener("click", () => {
      this.hideContextMenu();
      onClick();
    });
    menu.append(btn);
  }

  private addCtxSep(menu: HTMLElement): void {
    const sep = document.createElement("div");
    sep.className = "lw-ctx-sep";
    menu.append(sep);
  }

  private showHeaderContextMenu(x: number, y: number): void {
    this.openMenuAt(x, y, (menu) => {
      for (const col of ALL_OPTIONAL_COLUMNS) {
        const checked = this.columns.includes(col);
        this.addCtxItem(menu, `${checked ? "✓ " : "   "}${COLUMN_LABELS[col]}`, "", () => {
          const next = checked
            ? this.columns.filter((c) => c !== col)
            : ALL_OPTIONAL_COLUMNS.filter((c) => c === col || this.columns.includes(c));
          vscode.postMessage({ type: "setColumns", columns: next });
        });
      }
      this.addCtxSep(menu);
      this.addCtxItem(menu, "Reset Column Widths", "", () => {
        vscode.postMessage({ type: "setColumnWidths", widths: { ...DEFAULT_COLUMN_WIDTHS } });
      });
    });
  }

  private showRowContextMenu(row: WatchRowNode, x: number, y: number): void {
    const selectedRows = this.rows.filter((r) => this.selectedIds.has(r.id));
    const wpAllowed =
      Boolean(row.address) && !row.bitRange && (row.size === 1 || row.size === 2 || row.size === 4);

    this.openMenuAt(x, y, (menu) => {
      if (row.writable) {
        this.addCtxItem(menu, "Edit Value…", "F2", () => this.startInlineEdit(row.id, "value"));
      }
      if (row.parent === null) {
        this.addCtxItem(menu, "Edit Expression…", "", () => this.startInlineEdit(row.id, "expr"));
      }
      if (row.rawHex) {
        this.addCtxSep(menu);
        for (const opt of FORMAT_OPTIONS) {
          this.addCtxItem(
            menu,
            `${row.format === opt.key ? "✓ " : "   "}Format: ${opt.label}`,
            opt.short,
            () => vscode.postMessage({ type: "setFormat", id: row.id, format: opt.key }),
          );
        }
      }
      this.addCtxSep(menu);
      const plottable = selectedRows.filter((r) => r.plottable).map((r) => r.expr);
      this.addCtxItem(
        menu,
        "Add to Timeline (HSS)",
        "Ctrl+G",
        () => vscode.postMessage({ type: "addToTimeline", exprs: plottable }),
        plottable.length === 0,
      );

      if (row.address) {
        const addr = row.address;
        this.addCtxItem(menu, "View in Memory", "", () =>
          vscode.postMessage({ type: "viewMemory", address: addr, name: row.expr }),
        );
        if (row.watchpoint) {
          this.addCtxItem(menu, `Clear Data Breakpoint (${row.watchpoint})`, "", () =>
            vscode.postMessage({ type: "toggleWatchpoint", id: row.id, access: null }),
          );
        } else {
          const accesses: Array<{ label: string; access: WatchpointAccess }> = [
            { label: "Break on Write", access: "write" },
            { label: "Break on Read/Write", access: "readWrite" },
            { label: "Break on Read", access: "read" },
          ];
          for (const item of accesses) {
            this.addCtxItem(
              menu,
              item.label,
              wpAllowed ? "DWT" : "N/A",
              () => vscode.postMessage({ type: "toggleWatchpoint", id: row.id, access: item.access }),
              !wpAllowed,
            );
          }
        }
      }

      this.addCtxSep(menu);
      this.addCtxItem(menu, "Copy Expression = Value", "Ctrl+C", () =>
        vscode.postMessage({ type: "copy", text: `${row.expr} = ${formatScalarValue(row)}` }),
      );
      this.addCtxItem(menu, "Copy Value", "", () =>
        vscode.postMessage({ type: "copy", text: formatScalarValue(row) }),
      );
      if (row.rawHex) {
        const hex = row.rawHex;
        this.addCtxItem(menu, "Copy Hex", "", () => vscode.postMessage({ type: "copy", text: hex }));
      }
      if (row.address) {
        const addr = row.address;
        this.addCtxItem(menu, "Copy Address", "", () =>
          vscode.postMessage({ type: "copy", text: addr }),
        );
      }

      this.addCtxSep(menu);
      const isPopped = document.body.dataset.surface === "panel";
      this.addCtxItem(
        menu,
        isPopped ? "Dock to Bottom Panel" : "Pop Out to Editor Tab",
        "",
        () => vscode.postMessage({ type: isPopped ? "dockToPanel" : "popOut" }),
      );
      this.addCtxItem(
        menu,
        selectedRows.length > 1 ? `Remove ${selectedRows.length} Items` : "Remove",
        "Del",
        () => vscode.postMessage({ type: "remove", ids: [...this.selectedIds] }),
      );
    });
  }
}

const appRoot = document.getElementById("app");
if (appRoot) {
  new LiveWatchApp(appRoot);
}
