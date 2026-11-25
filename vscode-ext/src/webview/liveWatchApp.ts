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

const COLUMN_LABELS: Record<WatchColumnKey, string> = {
  hex: "Hex / Format",
  type: "Type",
  address: "Address",
  rate: "Rate",
};

const FORMAT_OPTIONS: Array<{ key: WatchFormat; short: string; label: string }> = [
  { key: "default", short: "AUTO", label: "Default" },
  { key: "dec", short: "DEC", label: "Signed Decimal" },
  { key: "uint", short: "UINT", label: "Unsigned Decimal" },
  { key: "hex", short: "HEX", label: "Hexadecimal" },
  { key: "bin", short: "BIN", label: "Binary (0b)" },
  { key: "float", short: "FLT", label: "IEEE-754 Float" },
  { key: "char", short: "CHR", label: "ASCII Char" },
];

const RATE_OPTIONS = [
  { value: 0, label: "Stop" },
  { value: 1, label: "1 Hz" },
  { value: 2, label: "2 Hz" },
  { value: 5, label: "5 Hz" },
  { value: 10, label: "10 Hz" },
  { value: 20, label: "20 Hz" },
];

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
  path.setAttribute("d", "M8 2.5a.6.6 0 0 1 .6.6v4.3h4.3a.6.6 0 0 1 0 1.2H8.6v4.3a.6.6 0 0 1-1.2 0V8.6H3.1a.6.6 0 0 1 0-1.2h4.3V3.1a.6.6 0 0 1 .6-.6z");
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
  path.setAttribute("d", "M3.5 3h9A1.5 1.5 0 0 1 14 4.5v7a1.5 1.5 0 0 1-1.5 1.5h-9A1.5 1.5 0 0 1 2 11.5v-7A1.5 1.5 0 0 1 3.5 3zm0 1.2a.3.3 0 0 0-.3.3v7a.3.3 0 0 0 .3.3h9a.3.3 0 0 0 .3-.3v-7a.3.3 0 0 0-.3-.3h-9zM5 6h2v1.4H5V6zm4 0h2v1.4H9V6zm-4 3h2v1.4H5V9zm4 0h2v1.4H9V9z");
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
  path.setAttribute("d", "M5 2.5A1.5 1.5 0 0 1 6.5 1h6A1.5 1.5 0 0 1 14 2.5v7A1.5 1.5 0 0 1 12.5 11H11v1.5A1.5 1.5 0 0 1 9.5 14h-6A1.5 1.5 0 0 1 2 12.5v-7A1.5 1.5 0 0 1 3.5 4H5V2.5zm1.2 1.5H9.5A1.5 1.5 0 0 1 11 5.5v4.3h1.5a.3.3 0 0 0 .3-.3v-7a.3.3 0 0 0-.3-.3h-6a.3.3 0 0 0-.3.3V4zM3.5 5.2a.3.3 0 0 0-.3.3v7a.3.3 0 0 0 .3.3h6a.3.3 0 0 0 .3-.3v-7a.3.3 0 0 0-.3-.3h-6z");
  svg.append(path);
  return svg;
}

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
        const buf = new ArrayBuffer(4);
        const view = new DataView(buf);
        view.setUint32(0, Number(uval & 0xffff_ffffn), true);
        const f = view.getFloat32(0, true);
        return Number.isFinite(f) ? String(Number(f.toPrecision(7))) : String(f);
      }
      const buf = new ArrayBuffer(8);
      const view = new DataView(buf);
      view.setBigUint64(0, uval, true);
      const d = view.getFloat64(0, true);
      return Number.isFinite(d) ? String(d) : String(d);
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
    lines.push(`Prev: ${row.prevValue}${row.prevRawHex ? ` (${row.prevRawHex})` : ""} -> Now: ${row.value}${ago}`);
  }
  if (row.error) lines.push(`Error: ${row.error}`);
  return lines.join("\n");
}

interface RowDom {
  el: HTMLElement;
  valCell: HTMLElement;
  valText: HTMLElement;
  hexText: HTMLElement | null;
  addrText: HTMLElement | null;
  rateSelect: HTMLSelectElement | null;
  fmtBtn: HTMLButtonElement | null;
  toggleAlt: boolean;
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

  private readonly rowDoms = new Map<number, RowDom>();
  private contextMenu: HTMLElement | null = null;

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
      this.renderBody();
    });
    this.filterInput.addEventListener("keydown", (e) => {
      if (e.key === "Escape") {
        e.preventDefault();
        this.filterText = "";
        this.filterInput.value = "";
        this.filterVisible = false;
        this.filterBar.hidden = true;
        this.renderBody();
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
    this.applyGridColumns();
    this.renderHeader();
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
        if (!this.compPopup.hidden && this.compActiveIndex >= 0 && this.completions[this.compActiveIndex]) {
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
    if (target && (target.tagName === "INPUT" || target.tagName === "SELECT" || target.tagName === "TEXTAREA")) {
      return;
    }
    if (this.editing) {
      return;
    }

    const mod = e.ctrlKey || e.metaKey;
    const visibleRows = this.filteredRows();
    const curIdx = this.focusedId !== null ? visibleRows.findIndex((r) => r.id === this.focusedId) : -1;
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
      const nextIdx = curIdx < 0 ? 0 : Math.max(0, Math.min(visibleRows.length - 1, curIdx + delta));
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

  private onHostMessage(msg: HostToWebviewMessage): void {
    switch (msg.type) {
      case "snapshot":
        this.rows = msg.rows;
        this.byId = new Map(msg.rows.map((r) => [r.id, r]));
        this.columns = msg.columns;
        this.widths = msg.widths;
        this.freshnessMs = msg.freshnessMs;
        this.runState = msg.runState;
        this.connected = msg.connected;
        this.applyFreshnessVar();
        this.applyGridColumns();
        this.renderHeader();
        this.renderStatusBanner();
        this.renderBody();
        break;
      case "patch":
        this.runState = msg.runState;
        this.connected = msg.connected;
        this.renderStatusBanner();
        for (const updated of msg.updates) {
          const idx = this.rows.findIndex((r) => r.id === updated.id);
          if (idx >= 0) {
            this.rows[idx] = updated;
          }
          this.byId.set(updated.id, updated);
          this.updateRowDom(updated);
        }
        break;
      case "state":
        this.runState = msg.runState;
        this.connected = msg.connected;
        this.freshnessMs = msg.freshnessMs;
        this.applyFreshnessVar();
        this.renderStatusBanner();
        for (const row of this.rows) {
          this.updateRowFreshness(row, this.rowDoms.get(row.id));
        }
        break;
      case "columns":
        this.columns = msg.columns;
        this.widths = msg.widths;
        this.applyGridColumns();
        this.renderHeader();
        this.renderBody();
        break;
      case "revealRow": {
        this.selectedIds = new Set([msg.id]);
        this.focusedId = msg.id;
        this.updateSelectionStyles();
        const dom = this.rowDoms.get(msg.id);
        if (dom) {
          dom.el.scrollIntoView({ block: "nearest" });
          dom.el.classList.remove("is-reveal-flash");
          void dom.el.offsetWidth;
          dom.el.classList.add("is-reveal-flash");
        }
        break;
      }
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
        this.renderStatusBanner();
        if (this.errorTimer !== undefined) {
          window.clearTimeout(this.errorTimer);
        }
        this.errorTimer = window.setTimeout(() => {
          this.errorMessage = null;
          this.renderStatusBanner();
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
    if (this.errorMessage) {
      this.statusBanner.hidden = false;
      this.statusBanner.className = "lw-status-banner is-error";
      this.statusBanner.textContent = this.errorMessage;
      this.bodyEl.classList.toggle("is-stale", !this.connected);
      return;
    }
    this.statusBanner.className = "lw-status-banner";
    this.bodyEl.classList.toggle("is-stale", !this.connected);
    if (!this.connected) {
      this.statusBanner.hidden = false;
      this.statusBanner.textContent = "Disconnected · Showing cached watch list (start debugging to stream live values)";
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
      hex: "Hex / Format",
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
          this.applyGridColumns();
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
    const matchingRoots = new Set<number>();
    const rootOf = (row: WatchRowNode): number => {
      let cur: WatchRowNode | undefined = row;
      while (cur && cur.parent !== null) {
        cur = this.byId.get(cur.parent);
      }
      return cur?.id ?? row.id;
    };
    for (const row of this.rows) {
      const hay = `${row.name} ${row.expr} ${row.type} ${row.value}`.toLowerCase();
      if (hay.includes(needle)) {
        matchingRoots.add(rootOf(row));
      }
    }
    return this.rows.filter((r) => matchingRoots.has(rootOf(r)));
  }

  private renderBody(): void {
    this.rowDoms.clear();
    const visible = this.filteredRows();
    const elements: HTMLElement[] = [];
    for (const row of visible) {
      const dom = this.createRowDom(row);
      this.rowDoms.set(row.id, dom);
      elements.push(dom.el);
    }
    this.bodyEl.replaceChildren(...elements);
    this.updateSelectionStyles();
  }

  private createRowDom(row: WatchRowNode): RowDom {
    const el = document.createElement("div");
    el.className = "lw-row";
    el.dataset.id = String(row.id);

    if (row.parent === null) {
      el.draggable = true;
      el.addEventListener("dragstart", (e) => {
        this.dragRootId = row.id;
        e.dataTransfer?.setData("text/plain", String(row.id));
      });
      el.addEventListener("dragover", (e) => {
        if (this.dragRootId !== null && this.dragRootId !== row.id) {
          e.preventDefault();
          el.classList.add("is-drop-above");
        }
      });
      el.addEventListener("dragleave", () => el.classList.remove("is-drop-above"));
      el.addEventListener("drop", (e) => {
        e.preventDefault();
        el.classList.remove("is-drop-above");
        if (this.dragRootId === null || this.dragRootId === row.id) return;
        const roots = this.rows.filter((r) => r.parent === null).map((r) => r.id);
        const fromIdx = roots.indexOf(this.dragRootId);
        const toIdx = roots.indexOf(row.id);
        this.dragRootId = null;
        if (fromIdx < 0 || toIdx < 0) return;
        const [moved] = roots.splice(fromIdx, 1);
        roots.splice(toIdx, 0, moved);
        vscode.postMessage({ type: "reorder", rootIds: roots });
      });
      el.addEventListener("dragend", () => {
        this.dragRootId = null;
      });
    }

    el.addEventListener("click", (e) => {
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

    el.addEventListener("contextmenu", (e) => {
      e.preventDefault();
      if (!this.selectedIds.has(row.id)) {
        this.selectedIds = new Set([row.id]);
        this.focusedId = row.id;
        this.updateSelectionStyles();
      }
      this.showRowContextMenu(row, e.clientX, e.clientY);
    });

    // 1. Expression cell
    const nameCell = document.createElement("div");
    nameCell.className = "lw-cell lw-cell-name";
    nameCell.style.paddingLeft = `${7 + row.depth * 14}px`;

    if (row.expandable) {
      const toggle = document.createElement("button");
      toggle.type = "button";
      toggle.className = "lw-toggle-btn";
      toggle.append(svgChevron(row.expanded));
      toggle.addEventListener("click", (e) => {
        e.stopPropagation();
        vscode.postMessage({ type: row.expanded ? "collapse" : "expand", id: row.id });
      });
      nameCell.append(toggle);
    } else {
      const sp = document.createElement("span");
      sp.className = "lw-toggle-spacer";
      nameCell.append(sp);
    }

    if (row.watchpoint) {
      const wp = document.createElement("span");
      wp.className = "lw-wp-dot";
      wp.title = `Hardware data breakpoint (${row.watchpoint})`;
      nameCell.append(wp);
    }

    const nameSpan = document.createElement("span");
    nameSpan.className = "lw-text-trunc lw-mono";
    nameSpan.textContent = row.name;
    nameSpan.title = formatTooltip(row);
    nameCell.append(nameSpan);

    if (row.plottable) {
      const hoverActs = document.createElement("span");
      hoverActs.className = "lw-hover-actions";
      const waveBtn = document.createElement("button");
      waveBtn.type = "button";
      waveBtn.className = "lw-mini-icon-btn";
      waveBtn.title = "Add to Timeline (Ctrl+G)";
      waveBtn.append(svgWaveform());
      waveBtn.addEventListener("click", (e) => {
        e.stopPropagation();
        vscode.postMessage({ type: "addToTimeline", exprs: [row.expr] });
      });
      hoverActs.append(waveBtn);
      nameCell.append(hoverActs);
    }

    if (row.parent === null) {
      nameCell.addEventListener("dblclick", (e) => {
        e.stopPropagation();
        this.startInlineEdit(row.id, "expr");
      });
    }

    // 2. Value cell
    const valCell = document.createElement("div");
    valCell.className = "lw-cell lw-cell-value";
    if (row.writable) valCell.classList.add("is-writable");
    if (row.error) valCell.classList.add("is-error");

    const valText = document.createElement("span");
    valText.className = "lw-text-trunc lw-mono lw-val-text";
    valText.textContent = formatScalarValue(row);
    valCell.title = formatTooltip(row);
    valCell.append(valText);

    if (row.writable) {
      valCell.addEventListener("dblclick", (e) => {
        e.stopPropagation();
        this.startInlineEdit(row.id, "value");
      });
    }

    el.append(nameCell, valCell);

    let hexText: HTMLElement | null = null;
    let fmtBtn: HTMLButtonElement | null = null;
    let addrText: HTMLElement | null = null;
    let rateSelect: HTMLSelectElement | null = null;

    for (const col of this.columns) {
      if (col === "hex") {
        const cell = document.createElement("div");
        cell.className = "lw-cell lw-cell-hex";
        hexText = document.createElement("span");
        hexText.className = "lw-text-trunc lw-mono";
        hexText.textContent = row.rawHex ?? "—";
        hexText.title = row.rawHex ?? "No scalar hex representation";
        cell.append(hexText);
        if (row.rawHex) {
          const fmtOpt = FORMAT_OPTIONS.find((f) => f.key === row.format) ?? FORMAT_OPTIONS[0];
          fmtBtn = document.createElement("button");
          fmtBtn.type = "button";
          fmtBtn.className = "lw-fmt-pill";
          fmtBtn.textContent = fmtOpt.short;
          fmtBtn.title = "Cycle display format (or right-click row)";
          fmtBtn.addEventListener("click", (e) => {
            e.stopPropagation();
            const idx = FORMAT_OPTIONS.findIndex((f) => f.key === (this.byId.get(row.id)?.format ?? "default"));
            const next = FORMAT_OPTIONS[(idx + 1) % FORMAT_OPTIONS.length].key;
            vscode.postMessage({ type: "setFormat", id: row.id, format: next });
          });
          cell.append(fmtBtn);
        }
        el.append(cell);
      } else if (col === "type") {
        const cell = document.createElement("div");
        cell.className = "lw-cell lw-cell-type";
        const span = document.createElement("span");
        span.className = "lw-text-trunc lw-mono";
        span.textContent = row.type ? `${row.type}${row.bitRange ? ` ${row.bitRange}` : ""}` : "—";
        span.title = span.textContent;
        cell.append(span);
        el.append(cell);
      } else if (col === "address") {
        const cell = document.createElement("div");
        cell.className = "lw-cell lw-cell-addr";
        addrText = document.createElement("span");
        addrText.className = "lw-text-trunc lw-mono";
        addrText.textContent = row.address ? `${row.address}${row.bitRange ?? ""}` : "—";
        addrText.title = addrText.textContent;
        cell.append(addrText);
        if (row.address) {
          const addrVal = row.address;
          const hover = document.createElement("span");
          hover.className = "lw-hover-actions";
          const memBtn = document.createElement("button");
          memBtn.type = "button";
          memBtn.className = "lw-mini-icon-btn";
          memBtn.title = "View in Memory";
          memBtn.append(svgMemory());
          memBtn.addEventListener("click", (e) => {
            e.stopPropagation();
            vscode.postMessage({ type: "viewMemory", address: addrVal, name: row.expr });
          });
          const copyBtn = document.createElement("button");
          copyBtn.type = "button";
          copyBtn.className = "lw-mini-icon-btn";
          copyBtn.title = "Copy address";
          copyBtn.append(svgCopy());
          copyBtn.addEventListener("click", (e) => {
            e.stopPropagation();
            vscode.postMessage({ type: "copy", text: addrVal });
          });
          hover.append(memBtn, copyBtn);
          cell.append(hover);
        }
        el.append(cell);
      } else if (col === "rate") {
        const cell = document.createElement("div");
        cell.className = "lw-cell lw-cell-rate";
        rateSelect = document.createElement("select");
        rateSelect.className = "lw-rate-select";
        for (const opt of RATE_OPTIONS) {
          const o = new Option(opt.label, String(opt.value));
          rateSelect.append(o);
        }
        rateSelect.value = String(row.rateHz);
        rateSelect.addEventListener("click", (e) => e.stopPropagation());
        rateSelect.addEventListener("change", () => {
          const hz = Number(rateSelect?.value ?? 5);
          vscode.postMessage({ type: "setRate", ids: [row.id], rateHz: hz });
        });
        cell.append(rateSelect);
        el.append(cell);
      }
    }

    const dom: RowDom = { el, valCell, valText, hexText, addrText, rateSelect, fmtBtn, toggleAlt: false };
    this.updateRowFreshness(row, dom);
    return dom;
  }

  private updateRowDom(row: WatchRowNode): void {
    const dom = this.rowDoms.get(row.id);
    if (!dom) {
      return;
    }
    if (this.editing && this.editing.id === row.id && this.editing.field === "value") {
      return;
    }
    dom.valCell.classList.toggle("is-error", Boolean(row.error));
    dom.valText.textContent = formatScalarValue(row);
    dom.valCell.title = formatTooltip(row);
    if (dom.hexText) {
      dom.hexText.textContent = row.rawHex ?? "—";
    }
    if (dom.fmtBtn) {
      const fmtOpt = FORMAT_OPTIONS.find((f) => f.key === row.format) ?? FORMAT_OPTIONS[0];
      dom.fmtBtn.textContent = fmtOpt.short;
    }
    if (dom.addrText) {
      dom.addrText.textContent = row.address ? `${row.address}${row.bitRange ?? ""}` : "—";
    }
    if (dom.rateSelect) {
      dom.rateSelect.value = String(row.rateHz);
    }
    this.updateRowFreshness(row, dom);
  }

  private updateRowFreshness(row: WatchRowNode, dom: RowDom | undefined): void {
    if (!dom) return;
    const cell = dom.valCell;
    cell.classList.remove("is-fresh-a", "is-fresh-b", "is-stop-changed");
    cell.style.animationDelay = "";

    if (!this.connected || row.error) {
      return;
    }
    if (this.runState === "halted" && row.changedOnStop) {
      cell.classList.add("is-stop-changed");
      return;
    }
    if (this.freshnessMs <= 0 || row.lastChangedAtMs === null) {
      return;
    }
    const elapsed = row.elapsedMs !== null ? row.elapsedMs : Math.max(0, Date.now() - row.lastChangedAtMs);
    if (elapsed >= this.freshnessMs) {
      return;
    }
    dom.toggleAlt = !dom.toggleAlt;
    cell.classList.add(dom.toggleAlt ? "is-fresh-a" : "is-fresh-b");
    cell.style.animationDelay = `-${Math.round(elapsed)}ms`;
  }

  private updateSelectionStyles(): void {
    for (const [id, dom] of this.rowDoms.entries()) {
      dom.el.classList.toggle("is-selected", this.selectedIds.has(id));
      dom.el.classList.toggle("is-focused", this.focusedId === id);
    }
  }

  private startInlineEdit(id: number, field: "value" | "expr"): void {
    const row = this.byId.get(id);
    const dom = this.rowDoms.get(id);
    if (!row || !dom) return;
    if (field === "value" && !row.writable) return;
    if (field === "expr" && row.parent !== null) return;

    this.editing = { id, field };
    const hostCell = field === "value" ? dom.valCell : dom.el.querySelector<HTMLElement>(".lw-cell-name");
    if (!hostCell) {
      this.editing = null;
      return;
    }

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
        if (latest) this.updateRowDom(latest);
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
        this.addCtxItem(menu, "Copy Address", "", () => vscode.postMessage({ type: "copy", text: addr }));
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
