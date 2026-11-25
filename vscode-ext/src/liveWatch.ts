import * as fs from "fs";
import * as path from "path";
import * as crypto from "crypto";
import * as vscode from "vscode";
import type { DataClient, WatchInfo, WatchValue } from "./dataClient";
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
  type WatchValuePatch,
  type WatchpointAccess,
  type WebviewToHostMessage,
} from "./shared/liveWatchProtocol";

const DEBUG_TYPE = "haltd";
const UPDATE_COALESCE_MS = 50;
const RATES = [
  { label: "Off", description: "Refresh only when target stops", rateHz: 0 },
  { label: "1 Hz", rateHz: 1 },
  { label: "2 Hz", rateHz: 2 },
  { label: "5 Hz (Default)", rateHz: 5 },
  { label: "10 Hz", rateHz: 10 },
  { label: "20 Hz", rateHz: 20 },
];

interface SavedWatch {
  expr: string;
  rateHz: number;
  format?: WatchFormat;
}

interface SavedState {
  watches?: SavedWatch[];
  liveWatch?: {
    visibleColumns?: WatchColumnKey[];
    columnWidths?: Partial<ColumnWidths>;
    formats?: Record<string, WatchFormat>;
  };
}

interface InternalNode {
  info: WatchInfo;
  key: string;
  depth: number;
  expanded: boolean;
  format: WatchFormat;
  prevValue: string | null;
  prevRawHex: string | null;
  lastChangedAtMs: number | null;
  changedOnStop: boolean;
}

interface WatchpointEntry {
  address: string;
  size: number;
  access: WatchpointAccess;
  expression?: string;
}

interface Surface {
  readonly id: string;
  readonly webview: vscode.Webview;
  isVisible(): boolean;
}

function isWatchFormat(value: unknown): value is WatchFormat {
  return (
    value === "default" ||
    value === "dec" ||
    value === "uint" ||
    value === "hex" ||
    value === "bin" ||
    value === "float" ||
    value === "char"
  );
}

function isColumnKey(value: unknown): value is WatchColumnKey {
  return value === "type" || value === "address" || value === "rate";
}

function formatMemoryDump(baseAddrStr: string, base64Data: string): string {
  const bytes = Buffer.from(base64Data, "base64");
  const base = Number.parseInt(baseAddrStr, 16) || 0;
  const lines: string[] = [];
  for (let offset = 0; offset < bytes.length; offset += 16) {
    const addr = (base + offset) >>> 0;
    const hexParts: string[] = [];
    let ascii = "";
    for (let i = 0; i < 16; i++) {
      if (offset + i < bytes.length) {
        const b = bytes[offset + i];
        hexParts.push(b.toString(16).padStart(2, "0"));
        ascii += b >= 0x20 && b <= 0x7e ? String.fromCharCode(b) : ".";
      } else {
        hexParts.push("  ");
        ascii += " ";
      }
      if (i === 7) {
        hexParts.push("");
      }
    }
    lines.push(`0x${addr.toString(16).padStart(8, "0")}:  ${hexParts.join(" ")}   |${ascii}|`);
  }
  return lines.join("\n");
}

export interface LiveWatchHostActions {
  onAddToTimeline(expr: string): Promise<void>;
  onError(error: unknown, context: string): void;
}

/** Single source of truth for Live Watch across the bottom Panel WebviewView
 *  and the popped-out editor WebviewPanel. */
export class LiveWatchController implements vscode.WebviewViewProvider, vscode.Disposable {
  private readonly nodes = new Map<number, InternalNode>();
  private rootIds: number[] = [];
  private readonly children = new Map<number, number[]>();
  private readonly expandedKeys = new Set<string>();
  private readonly formats = new Map<string, WatchFormat>();
  private readonly watchpointsByAddr = new Map<string, WatchpointAccess>();

  private visibleColumns: WatchColumnKey[];
  private columnWidths: ColumnWidths = { ...DEFAULT_COLUMN_WIDTHS };
  private runState: TargetRunState = "disconnected";
  private nextOfflineId = -1;

  private readonly dirty = new Set<number>();
  private timer: NodeJS.Timeout | undefined;
  private view: vscode.WebviewView | undefined;
  private panel: vscode.WebviewPanel | undefined;
  private readonly disposables: vscode.Disposable[] = [];

  constructor(
    private readonly extensionUri: vscode.Uri,
    private readonly client: DataClient,
    private readonly actions: LiveWatchHostActions,
  ) {
    this.visibleColumns = this.configuredColumns();
    this.loadSavedState();

    client.on("update", (values: WatchValue[]) => this.onUpdate(values));
    client.on("close", () => {
      if (this.runState !== "disconnected") {
        this.setRunState("disconnected");
      }
    });

    this.disposables.push(
      vscode.workspace.onDidChangeConfiguration((event) => {
        if (event.affectsConfiguration("haltd.liveWatch.columns")) {
          this.visibleColumns = this.configuredColumns();
          this.broadcast({ type: "columns", columns: this.visibleColumns, widths: this.columnWidths });
        } else if (event.affectsConfiguration("haltd.liveWatch.freshnessDurationMs")) {
          this.broadcast({
            type: "state",
            runState: this.runState,
            connected: this.client.connected,
            freshnessMs: this.freshnessMs(),
          });
        }
      }),
    );
  }

  private configuredColumns(): WatchColumnKey[] {
    const raw = vscode.workspace.getConfiguration("haltd").get<unknown[]>("liveWatch.columns");
    if (!Array.isArray(raw)) {
      return [...ALL_OPTIONAL_COLUMNS];
    }
    return ALL_OPTIONAL_COLUMNS.filter((col) => raw.includes(col));
  }

  private freshnessMs(): number {
    const ms = vscode.workspace.getConfiguration("haltd").get<number>("liveWatch.freshnessDurationMs", 2500);
    return Number.isFinite(ms) ? Math.max(0, Math.min(10000, ms)) : 2500;
  }

  private stateFile(): string | undefined {
    const folder = vscode.workspace.workspaceFolders?.[0];
    return folder && path.join(folder.uri.fsPath, ".haltd", "state.json");
  }

  private readStateFile(): SavedState {
    const file = this.stateFile();
    if (!file) {
      return {};
    }
    try {
      return JSON.parse(fs.readFileSync(file, "utf8")) as SavedState;
    } catch {
      return {};
    }
  }

  private loadSavedState(): void {
    const saved = this.readStateFile();
    if (saved.liveWatch?.visibleColumns) {
      this.visibleColumns = ALL_OPTIONAL_COLUMNS.filter((c) => saved.liveWatch?.visibleColumns?.includes(c));
    }
    if (saved.liveWatch?.columnWidths) {
      this.columnWidths = {
        ...DEFAULT_COLUMN_WIDTHS,
        ...saved.liveWatch.columnWidths,
      };
    }
    if (saved.liveWatch?.formats) {
      for (const [k, v] of Object.entries(saved.liveWatch.formats)) {
        if (isWatchFormat(v)) {
          this.formats.set(k, v);
        }
      }
    }
    const watches = saved.watches ?? [];
    for (const item of watches) {
      if (item.format && isWatchFormat(item.format)) {
        this.formats.set(item.expr, item.format);
      }
      this.createOfflineRoot(item.expr, item.rateHz ?? 5);
    }
  }

  private createOfflineRoot(expr: string, rateHz: number): InternalNode {
    const id = this.nextOfflineId--;
    const node: InternalNode = {
      info: {
        id,
        parent: null,
        name: expr,
        expr,
        type: "",
        expandable: false,
        writable: false,
        rateHz,
        value: "—",
        error: "Not connected to debug session",
        plottable: false,
        address: null,
        bitRange: null,
        size: null,
        rawHex: null,
        scalarKind: null,
      },
      key: expr,
      depth: 0,
      expanded: false,
      format: this.formats.get(expr) ?? "default",
      prevValue: null,
      prevRawHex: null,
      lastChangedAtMs: null,
      changedOnStop: false,
    };
    this.nodes.set(id, node);
    this.rootIds.push(id);
    return node;
  }

  private saveState(): void {
    const file = this.stateFile();
    if (!file) {
      return;
    }
    fs.mkdirSync(path.dirname(file), { recursive: true });
    const watches: SavedWatch[] = this.rootIds
      .map((id) => this.nodes.get(id))
      .filter((n): n is InternalNode => Boolean(n))
      .map((n) => ({
        expr: n.info.expr,
        rateHz: n.info.rateHz,
        ...(n.format !== "default" ? { format: n.format } : {}),
      }));
    const formats: Record<string, WatchFormat> = {};
    for (const [k, v] of this.formats.entries()) {
      if (v !== "default") {
        formats[k] = v;
      }
    }
    const payload: SavedState = {
      watches,
      liveWatch: {
        visibleColumns: this.visibleColumns,
        columnWidths: this.columnWidths,
        formats,
      },
    };
    fs.writeFileSync(file, JSON.stringify(payload, null, 2) + "\n");
  }

  private nodeKey(info: WatchInfo): string {
    if (info.parent === null) {
      return info.expr;
    }
    const parent = this.nodes.get(info.parent);
    return `${parent ? parent.key : info.parent}/${info.name}`;
  }

  private toRow(node: InternalNode, now: number): WatchRowNode {
    const addrKey = node.info.address?.toLowerCase();
    const watchpoint = addrKey ? (this.watchpointsByAddr.get(addrKey) ?? null) : null;
    const elapsedMs = node.lastChangedAtMs !== null ? Math.max(0, now - node.lastChangedAtMs) : null;
    return {
      ...node.info,
      key: node.key,
      depth: node.depth,
      expanded: node.expanded,
      format: node.format,
      prevValue: node.prevValue,
      prevRawHex: node.prevRawHex,
      lastChangedAtMs: node.lastChangedAtMs,
      elapsedMs,
      changedOnStop: node.changedOnStop,
      watchpoint,
    };
  }

  private toValuePatch(node: InternalNode, now: number): WatchValuePatch {
    const elapsedMs = node.lastChangedAtMs !== null ? Math.max(0, now - node.lastChangedAtMs) : null;
    return {
      id: node.info.id,
      value: node.info.value,
      rawHex: node.info.rawHex,
      address: node.info.address,
      error: node.info.error,
      prevValue: node.prevValue,
      prevRawHex: node.prevRawHex,
      lastChangedAtMs: node.lastChangedAtMs,
      elapsedMs,
      changedOnStop: node.changedOnStop,
    };
  }

  private orderedRows(): WatchRowNode[] {
    const now = Date.now();
    const out: WatchRowNode[] = [];
    const visit = (id: number) => {
      const node = this.nodes.get(id);
      if (!node) {
        return;
      }
      out.push(this.toRow(node, now));
      if (node.expanded) {
        for (const childId of this.children.get(id) ?? []) {
          visit(childId);
        }
      }
    };
    for (const rootId of this.rootIds) {
      visit(rootId);
    }
    return out;
  }

  private activeSurfaces(): Surface[] {
    const list: Surface[] = [];
    if (this.view) {
      list.push({
        id: "view",
        webview: this.view.webview,
        isVisible: () => this.view?.visible ?? false,
      });
    }
    if (this.panel) {
      list.push({
        id: "panel",
        webview: this.panel.webview,
        isVisible: () => this.panel?.visible ?? false,
      });
    }
    return list;
  }

  private visibleSurfaces(): Surface[] {
    return this.activeSurfaces().filter((surface) => surface.isVisible());
  }

  private sendTo(webview: vscode.Webview, message: HostToWebviewMessage): void {
    void webview.postMessage(message);
  }

  private broadcast(message: HostToWebviewMessage): void {
    for (const surface of this.activeSurfaces()) {
      this.sendTo(surface.webview, message);
    }
  }

  private broadcastVisible(message: HostToWebviewMessage, exclude?: vscode.Webview): void {
    for (const surface of this.visibleSurfaces()) {
      if (surface.webview !== exclude) {
        this.sendTo(surface.webview, message);
      }
    }
  }

  private sendSnapshot(webview?: vscode.Webview): void {
    if (!webview && this.visibleSurfaces().length === 0) {
      return;
    }
    const msg: HostToWebviewMessage = {
      type: "snapshot",
      rows: this.orderedRows(),
      columns: this.visibleColumns,
      widths: this.columnWidths,
      freshnessMs: this.freshnessMs(),
      runState: this.runState,
      connected: this.client.connected,
    };
    if (webview) {
      this.sendTo(webview, msg);
    } else {
      this.broadcastVisible(msg);
    }
  }

  private onUpdate(values: WatchValue[]): void {
    const now = Date.now();
    for (const { id, value, rawHex, address, error } of values) {
      const node = this.nodes.get(id);
      if (!node) {
        continue;
      }
      const changed =
        node.info.value !== value ||
        node.info.rawHex !== rawHex ||
        node.info.address !== address ||
        node.info.error !== error;
      if (!changed) {
        continue;
      }
      const valueChanged = node.info.value !== value || node.info.rawHex !== rawHex;
      if (valueChanged) {
        node.prevValue = node.info.value;
        node.prevRawHex = node.info.rawHex;
        if (!error) {
          node.lastChangedAtMs = now;
          if (this.runState === "halted") {
            node.changedOnStop = true;
          }
        }
      }
      node.info.value = value;
      node.info.rawHex = rawHex;
      node.info.address = address;
      node.info.error = error;
      this.dirty.add(id);
    }
    if (this.dirty.size > 0) {
      if (this.visibleSurfaces().length === 0) {
        this.dirty.clear();
        return;
      }
      if (!this.timer) {
        this.timer = setTimeout(() => this.flush(), UPDATE_COALESCE_MS);
      }
    }
  }

  private flush(): void {
    this.timer = undefined;
    if (this.dirty.size === 0) {
      return;
    }
    if (this.visibleSurfaces().length === 0) {
      this.dirty.clear();
      return;
    }
    const now = Date.now();
    const updates: WatchValuePatch[] = [];
    for (const id of this.dirty) {
      const node = this.nodes.get(id);
      if (node) {
        updates.push(this.toValuePatch(node, now));
      }
    }
    this.dirty.clear();
    if (updates.length > 0) {
      this.broadcastVisible({
        type: "patch",
        updates,
        runState: this.runState,
        connected: this.client.connected,
      });
    }
  }

  setRunState(nextState: TargetRunState): void {
    const prev = this.runState;
    this.runState = nextState;
    if (nextState === "running" && prev !== "running") {
      for (const node of this.nodes.values()) {
        node.changedOnStop = false;
      }
      this.broadcastVisible({
        type: "state",
        runState: this.runState,
        connected: this.client.connected,
        freshnessMs: this.freshnessMs(),
      });
      return;
    }
    if (nextState === "disconnected") {
      for (const node of this.nodes.values()) {
        node.lastChangedAtMs = null;
        node.changedOnStop = false;
      }
      this.watchpointsByAddr.clear();
      this.sendSnapshot();
      return;
    }
    this.broadcastVisible({
      type: "state",
      runState: this.runState,
      connected: this.client.connected,
      freshnessMs: this.freshnessMs(),
    });
  }

  async syncWatchpoints(): Promise<void> {
    const session = vscode.debug.activeDebugSession;
    if (!session || session.type !== DEBUG_TYPE) {
      return;
    }
    try {
      const reply = (await session.customRequest("haltd/watchpoint", { action: "list" })) as {
        watchpoints?: WatchpointEntry[];
      };
      this.watchpointsByAddr.clear();
      for (const wp of reply.watchpoints ?? []) {
        this.watchpointsByAddr.set(wp.address.toLowerCase(), wp.access);
      }
      this.broadcastVisible({
        type: "metaPatch",
        rows: this.orderedRows().filter((r) => r.address !== null),
        runState: this.runState,
        connected: this.client.connected,
      });
    } catch {
      // Ignore if session is busy or ending
    }
  }

  async sync(): Promise<void> {
    if (!this.client.connected) {
      return;
    }
    const localRoots = this.rootIds
      .map((id) => this.nodes.get(id))
      .filter((n): n is InternalNode => Boolean(n))
      .map((n) => ({ expr: n.info.expr, rateHz: n.info.rateHz }));

    let list = await this.client.call<WatchInfo[]>("watch.list");
    if (list.length === 0 && localRoots.length > 0) {
      for (const saved of localRoots) {
        await this.client.call("watch.add", { expr: saved.expr, rateHz: saved.rateHz }).catch(() => undefined);
      }
      list = await this.client.call<WatchInfo[]>("watch.list");
    }

    const orderHint = new Map<string, number>();
    localRoots.forEach((item, idx) => orderHint.set(item.expr, idx));

    this.nodes.clear();
    this.children.clear();
    this.dirty.clear();

    const roots = list.filter((item) => item.parent === null);
    roots.sort((a, b) => {
      const ia = orderHint.get(a.expr);
      const ib = orderHint.get(b.expr);
      if (ia !== undefined && ib !== undefined) {
        return ia - ib;
      }
      if (ia !== undefined) {
        return -1;
      }
      if (ib !== undefined) {
        return 1;
      }
      return a.id - b.id;
    });

    this.rootIds = [];
    for (const info of roots) {
      const node: InternalNode = {
        info,
        key: info.expr,
        depth: 0,
        expanded: false,
        format: this.formats.get(info.expr) ?? "default",
        prevValue: null,
        prevRawHex: null,
        lastChangedAtMs: null,
        changedOnStop: false,
      };
      this.nodes.set(info.id, node);
      this.rootIds.push(info.id);
    }

    // Re-expand any nodes that were open before reload/rebind.
    const toExpand = [...this.rootIds];
    while (toExpand.length > 0) {
      const id = toExpand.shift()!;
      const node = this.nodes.get(id);
      if (!node || !node.info.expandable || !this.expandedKeys.has(node.key)) {
        continue;
      }
      try {
        const members = await this.client.call<WatchInfo[]>("watch.expand", { id });
        node.expanded = true;
        const childIds: number[] = [];
        for (const member of members) {
          const key = `${node.key}/${member.name}`;
          const childNode: InternalNode = {
            info: member,
            key,
            depth: node.depth + 1,
            expanded: false,
            format: this.formats.get(key) ?? "default",
            prevValue: null,
            prevRawHex: null,
            lastChangedAtMs: null,
            changedOnStop: false,
          };
          this.nodes.set(member.id, childNode);
          childIds.push(member.id);
          if (member.expandable && this.expandedKeys.has(key)) {
            toExpand.push(member.id);
          }
        }
        this.children.set(id, childIds);
      } catch {
        // Ignore nodes whose members can no longer be expanded
      }
    }

    if (this.runState === "disconnected") {
      this.runState = "running";
    }
    this.sendSnapshot();
    void this.syncWatchpoints();
  }

  async add(expr: string, reveal = false): Promise<WatchRowNode> {
    const trimmed = expr.trim();
    const existingId = this.rootIds.find((id) => this.nodes.get(id)?.info.expr === trimmed);
    if (existingId !== undefined) {
      const existing = this.nodes.get(existingId)!;
      if (reveal) {
        await this.revealSurfaceAndRow(existingId);
      }
      return this.toRow(existing, Date.now());
    }

    let node: InternalNode;
    if (this.client.connected) {
      const info = await this.client.call<WatchInfo>("watch.add", { expr: trimmed });
      node = {
        info,
        key: info.expr,
        depth: 0,
        expanded: false,
        format: this.formats.get(info.expr) ?? "default",
        prevValue: null,
        prevRawHex: null,
        lastChangedAtMs: null,
        changedOnStop: false,
      };
      this.nodes.set(info.id, node);
      this.rootIds.push(info.id);
    } else {
      node = this.createOfflineRoot(trimmed, 5);
    }

    this.saveState();
    this.sendSnapshot();
    if (reveal) {
      await this.revealSurfaceAndRow(node.info.id);
    }
    return this.toRow(node, Date.now());
  }

  private async revealSurfaceAndRow(id: number): Promise<void> {
    if (this.panel && this.panel.visible) {
      this.panel.reveal(undefined, true);
    } else if (this.view) {
      this.view.show(true);
    } else {
      await vscode.commands.executeCommand("haltd.liveWatch.focus");
    }
    this.broadcast({ type: "revealRow", id });
  }

  async modify(id: number, newExpr: string): Promise<void> {
    const trimmed = newExpr.trim();
    const node = this.nodes.get(id);
    if (!node || node.info.parent !== null) {
      return;
    }
    if (!trimmed) {
      await this.removeIds([id]);
      return;
    }
    if (trimmed === node.info.expr) {
      return;
    }
    this.forgetDescendants(id);
    this.expandedKeys.delete(node.key);
    if (this.client.connected && id > 0) {
      const info = await this.client.call<WatchInfo>("watch.modify", { id, expr: trimmed });
      node.info = info;
      node.key = info.expr;
      node.expanded = false;
      node.format = this.formats.get(info.expr) ?? "default";
      node.prevValue = null;
      node.prevRawHex = null;
      node.lastChangedAtMs = null;
      node.changedOnStop = false;
    } else {
      node.info.expr = trimmed;
      node.info.name = trimmed;
      node.key = trimmed;
    }
    this.saveState();
    this.sendSnapshot();
  }

  private forgetDescendants(id: number): void {
    for (const childId of this.children.get(id) ?? []) {
      this.forgetDescendants(childId);
      this.nodes.delete(childId);
      this.dirty.delete(childId);
    }
    this.children.delete(id);
  }

  async removeIds(ids: number[]): Promise<void> {
    const idSet = new Set(ids);
    const rootsToRemove = ids.filter((id) => {
      const node = this.nodes.get(id);
      return node && (node.info.parent === null || !idSet.has(node.info.parent));
    });
    for (const id of rootsToRemove) {
      const node = this.nodes.get(id);
      if (!node) {
        continue;
      }
      if (this.client.connected && id > 0) {
        await this.client.call("watch.remove", { id }).catch(() => undefined);
      }
      this.forgetDescendants(id);
      this.nodes.delete(id);
      this.dirty.delete(id);
      if (node.info.parent === null) {
        this.rootIds = this.rootIds.filter((rootId) => rootId !== id);
      } else {
        const siblings = this.children.get(node.info.parent) ?? [];
        this.children.set(
          node.info.parent,
          siblings.filter((siblingId) => siblingId !== id),
        );
      }
    }
    this.saveState();
    this.sendSnapshot();
  }

  async expand(id: number): Promise<void> {
    const node = this.nodes.get(id);
    if (!node || !this.client.connected || id <= 0) {
      return;
    }
    this.forgetDescendants(id);
    const members = await this.client.call<WatchInfo[]>("watch.expand", { id });
    node.expanded = true;
    this.expandedKeys.add(node.key);
    const childIds: number[] = [];
    for (const member of members) {
      const key = `${node.key}/${member.name}`;
      const child: InternalNode = {
        info: member,
        key,
        depth: node.depth + 1,
        expanded: false,
        format: this.formats.get(key) ?? "default",
        prevValue: null,
        prevRawHex: null,
        lastChangedAtMs: null,
        changedOnStop: false,
      };
      this.nodes.set(member.id, child);
      childIds.push(member.id);
    }
    this.children.set(id, childIds);
    this.sendSnapshot();
  }

  async collapse(id: number): Promise<void> {
    const node = this.nodes.get(id);
    if (!node) {
      return;
    }
    node.expanded = false;
    this.expandedKeys.delete(node.key);
    this.forgetDescendants(id);
    if (this.client.connected && id > 0) {
      await this.client.call("watch.collapse", { id }).catch(() => undefined);
    }
    this.sendSnapshot();
  }

  async setRate(ids: number[], rateHz: number): Promise<void> {
    const now = Date.now();
    const updatedRows: WatchRowNode[] = [];
    for (const id of ids) {
      const node = this.nodes.get(id);
      if (!node) {
        continue;
      }
      if (this.client.connected && id > 0) {
        await this.client.call("watch.setRate", { id, rateHz });
      }
      const applySubtree = (currId: number) => {
        const curr = this.nodes.get(currId);
        if (!curr) {
          return;
        }
        curr.info.rateHz = rateHz;
        updatedRows.push(this.toRow(curr, now));
        for (const childId of this.children.get(currId) ?? []) {
          applySubtree(childId);
        }
      };
      applySubtree(id);
    }
    this.saveState();
    if (updatedRows.length > 0) {
      this.broadcastVisible({
        type: "metaPatch",
        rows: updatedRows,
        runState: this.runState,
        connected: this.client.connected,
      });
    }
  }

  setFormat(id: number, format: WatchFormat): void {
    const node = this.nodes.get(id);
    if (!node) {
      return;
    }
    node.format = format;
    if (format === "default") {
      this.formats.delete(node.key);
    } else {
      this.formats.set(node.key, format);
    }
    this.saveState();
    this.broadcastVisible({
      type: "metaPatch",
      rows: [this.toRow(node, Date.now())],
      runState: this.runState,
      connected: this.client.connected,
    });
  }

  reorder(newRootIds: number[]): void {
    const valid = newRootIds.filter((id) => this.rootIds.includes(id));
    const remaining = this.rootIds.filter((id) => !valid.includes(id));
    this.rootIds = [...valid, ...remaining];
    this.saveState();
    this.sendSnapshot();
  }

  async write(id: number, value: string): Promise<void> {
    if (!this.client.connected || id <= 0) {
      throw new Error("Debug session is not connected");
    }
    await this.client.call("watch.write", { id, value });
  }

  async configureColumnsPicker(): Promise<void> {
    const labels: Record<WatchColumnKey, string> = {
      type: "Type",
      address: "Address",
      rate: "Refresh Rate",
    };
    const items = ALL_OPTIONAL_COLUMNS.map((key) => ({
      label: labels[key],
      key,
      picked: this.visibleColumns.includes(key),
    }));
    const picked = await vscode.window.showQuickPick(items, {
      canPickMany: true,
      placeHolder: "Select optional Live Watch columns (Expression & Value are always shown)",
    });
    if (!picked) {
      return;
    }
    const next = ALL_OPTIONAL_COLUMNS.filter((key) => picked.some((item) => item.key === key));
    await this.updateVisibleColumns(next);
  }

  private async updateVisibleColumns(columns: WatchColumnKey[]): Promise<void> {
    this.visibleColumns = ALL_OPTIONAL_COLUMNS.filter((col) => columns.includes(col));
    this.saveState();
    this.broadcast({ type: "columns", columns: this.visibleColumns, widths: this.columnWidths });
    await vscode.workspace
      .getConfiguration("haltd")
      .update("liveWatch.columns", this.visibleColumns, vscode.ConfigurationTarget.Workspace)
      .then(undefined, () => undefined);
  }

  async focusInputOrReveal(): Promise<void> {
    if (this.panel && this.panel.visible) {
      this.panel.reveal();
    } else if (this.view) {
      this.view.show(false);
    } else {
      await vscode.commands.executeCommand("haltd.liveWatch.focus");
    }
    this.broadcast({ type: "focusAddInput" });
  }

  async revealPanel(preserveFocus = false): Promise<void> {
    if (this.panel && this.panel.visible) {
      this.panel.reveal(undefined, preserveFocus);
      this.sendSnapshot(this.panel.webview);
      return;
    }
    if (this.view) {
      this.view.show(preserveFocus);
      this.sendSnapshot(this.view.webview);
      return;
    }
    await vscode.commands.executeCommand("haltd.liveWatch.focus");
  }

  revealOnDebugStart(): void {
    const autoOpen = vscode.workspace.getConfiguration("haltd").get<boolean>("liveWatch.openOnDebugStart", true);
    if (!autoOpen || vscode.debug.activeDebugSession?.type !== DEBUG_TYPE) {
      return;
    }
    if (this.panel) {
      this.panel.reveal(undefined, true);
      this.sendSnapshot(this.panel.webview);
      return;
    }
    if (this.view) {
      this.view.show(true);
      this.sendSnapshot(this.view.webview);
    } else {
      void vscode.commands.executeCommand("haltd.liveWatch.focus").then(undefined, () => undefined);
    }
  }

  async popOut(): Promise<void> {
    if (this.panel) {
      this.panel.reveal(vscode.ViewColumn.Beside);
      this.sendSnapshot(this.panel.webview);
      return;
    }
    const panel = vscode.window.createWebviewPanel(
      "haltd.liveWatchPanel",
      "Live Watch",
      vscode.ViewColumn.Beside,
      {
        enableScripts: true,
        retainContextWhenHidden: true,
        localResourceRoots: [vscode.Uri.joinPath(this.extensionUri, "dist")],
      },
    );
    panel.iconPath = vscode.Uri.joinPath(this.extensionUri, "resources", "livewatch.svg");
    this.panel = panel;
    panel.webview.onDidReceiveMessage((msg: WebviewToHostMessage) => {
      void this.handleMessage(msg, panel.webview);
    });
    panel.onDidChangeViewState(() => {
      if (panel.visible) {
        this.sendSnapshot(panel.webview);
      }
    });
    panel.onDidDispose(() => {
      if (this.panel === panel) {
        this.panel = undefined;
      }
    });
    panel.webview.html = this.buildHtml(panel.webview, "panel");
  }

  async dockToPanel(): Promise<void> {
    this.panel?.dispose();
    this.panel = undefined;
    if (this.view) {
      this.view.show(false);
      this.sendSnapshot(this.view.webview);
    } else {
      await vscode.commands.executeCommand("haltd.liveWatch.focus");
    }
  }

  resolveWebviewView(webviewView: vscode.WebviewView): void {
    this.view = webviewView;
    webviewView.webview.options = {
      enableScripts: true,
      localResourceRoots: [vscode.Uri.joinPath(this.extensionUri, "dist")],
    };
    webviewView.webview.onDidReceiveMessage((msg: WebviewToHostMessage) => {
      void this.handleMessage(msg, webviewView.webview);
    });
    webviewView.onDidChangeVisibility(() => {
      if (webviewView.visible) {
        this.sendSnapshot(webviewView.webview);
      }
    });
    webviewView.onDidDispose(() => {
      if (this.view === webviewView) {
        this.view = undefined;
      }
    });
    webviewView.webview.html = this.buildHtml(webviewView.webview, "panelView");
    this.sendSnapshot(webviewView.webview);
  }

  private async handleMessage(msg: WebviewToHostMessage, sender: vscode.Webview): Promise<void> {
    try {
      switch (msg.type) {
        case "ready":
          this.sendSnapshot(sender);
          break;
        case "add":
          await this.add(msg.expr, false);
          break;
        case "modify":
          await this.modify(msg.id, msg.expr);
          break;
        case "remove":
          await this.removeIds(msg.ids);
          break;
        case "write":
          await this.write(msg.id, msg.value);
          break;
        case "expand":
          await this.expand(msg.id);
          break;
        case "collapse":
          await this.collapse(msg.id);
          break;
        case "setRate":
          await this.setRate(msg.ids, msg.rateHz);
          break;
        case "setFormat":
          this.setFormat(msg.id, msg.format);
          break;
        case "reorder":
          this.reorder(msg.rootIds);
          break;
        case "setColumns":
          await this.updateVisibleColumns(msg.columns.filter(isColumnKey));
          break;
        case "setColumnWidths":
          this.columnWidths = { ...DEFAULT_COLUMN_WIDTHS, ...msg.widths };
          this.saveState();
          this.broadcastVisible({ type: "columnWidths", widths: this.columnWidths }, sender);
          break;
        case "complete": {
          if (!this.client.connected) {
            this.sendTo(sender, { type: "completions", requestId: msg.requestId, items: [] });
            return;
          }
          const reply = await this.client.call<{ items: SymbolCompletionItem[] }>("symbols.complete", {
            query: msg.query,
            limit: 40,
          });
          this.sendTo(sender, {
            type: "completions",
            requestId: msg.requestId,
            items: reply.items ?? [],
          });
          break;
        }
        case "addToTimeline":
          for (const expr of msg.exprs) {
            await this.actions.onAddToTimeline(expr);
          }
          break;
        case "viewMemory":
          await this.openMemoryView(msg.address, msg.name);
          break;
        case "toggleWatchpoint":
          await this.toggleWatchpoint(msg.id, msg.access);
          break;
        case "copy":
          await vscode.env.clipboard.writeText(msg.text);
          break;
        case "popOut":
          await this.popOut();
          break;
        case "dockToPanel":
          await this.dockToPanel();
          break;
      }
    } catch (error) {
      const message = error instanceof Error ? error.message : String(error);
      this.sendTo(sender, { type: "actionError", message });
    }
  }

  private async openMemoryView(address: string, name: string): Promise<void> {
    const session = vscode.debug.activeDebugSession;
    if (!session || session.type !== DEBUG_TYPE) {
      throw new Error("No active Haltd debug session");
    }
    const commands = await vscode.commands.getCommands(true);
    if (commands.includes("workbench.debug.viewlet.action.viewMemory")) {
      try {
        await vscode.commands.executeCommand("workbench.debug.viewlet.action.viewMemory", {
          sessionId: session.id,
          variable: { memoryReference: address },
        });
        return;
      } catch {
        // Fall through to built-in hex preview if hex editor extension is absent
      }
    }
    const reply = (await session.customRequest("readMemory", {
      memoryReference: address,
      offset: 0,
      count: 256,
    })) as { address?: string; data?: string };
    const dump = reply.data
      ? formatMemoryDump(reply.address ?? address, reply.data)
      : `${address}: <unreadable>`;
    const doc = await vscode.workspace.openTextDocument({
      language: "plaintext",
      content: `// Memory snapshot for ${name} @ ${reply.address ?? address} (256 bytes)\n\n${dump}\n`,
    });
    await vscode.window.showTextDocument(doc, { preview: true });
  }

  private async toggleWatchpoint(id: number, access: WatchpointAccess | null): Promise<void> {
    const node = this.nodes.get(id);
    if (!node || !node.info.address) {
      throw new Error("This watch entry has no target memory address");
    }
    const session = vscode.debug.activeDebugSession;
    if (!session || session.type !== DEBUG_TYPE) {
      throw new Error("No active Haltd debug session");
    }
    const reply = (await session.customRequest(
      "haltd/watchpoint",
      access === null
        ? { action: "remove", address: node.info.address }
        : {
            action: "set",
            address: node.info.address,
            size: node.info.size ?? 4,
            access,
            expression: node.info.expr,
          },
    )) as { watchpoints?: WatchpointEntry[] };
    this.watchpointsByAddr.clear();
    for (const wp of reply.watchpoints ?? []) {
      this.watchpointsByAddr.set(wp.address.toLowerCase(), wp.access);
    }
    this.broadcastVisible({
      type: "metaPatch",
      rows: this.orderedRows().filter((r) => r.address !== null),
      runState: this.runState,
      connected: this.client.connected,
    });
  }

  private buildHtml(webview: vscode.Webview, surfaceKind: "panelView" | "panel"): string {
    const nonce = crypto.randomBytes(16).toString("hex");
    const scriptUri = webview.asWebviewUri(vscode.Uri.joinPath(this.extensionUri, "dist", "liveWatchApp.js"));
    const styleUri = webview.asWebviewUri(vscode.Uri.joinPath(this.extensionUri, "dist", "liveWatchApp.css"));
    const csp = [
      "default-src 'none'",
      `style-src ${webview.cspSource} 'unsafe-inline'`,
      `script-src 'nonce-${nonce}'`,
      `img-src ${webview.cspSource}`,
    ].join("; ");

    return `<!doctype html>
<html lang="en">
  <head>
    <meta charset="utf-8" />
    <meta http-equiv="Content-Security-Policy" content="${csp}" />
    <meta name="viewport" content="width=device-width, initial-scale=1.0" />
    <title>Live Watch</title>
    <link rel="stylesheet" href="${styleUri}" />
  </head>
  <body data-surface="${surfaceKind}">
    <div id="app"></div>
    <script nonce="${nonce}" src="${scriptUri}"></script>
  </body>
</html>`;
  }

  dispose(): void {
    if (this.timer) {
      clearTimeout(this.timer);
      this.timer = undefined;
    }
    for (const d of this.disposables) {
      d.dispose();
    }
    this.panel?.dispose();
  }
}

export { RATES as LIVE_WATCH_RATES };
