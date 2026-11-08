import * as fs from "fs";
import * as path from "path";
import * as vscode from "vscode";
import type { DataClient, WatchInfo, WatchValue } from "./dataClient";

const UPDATE_COALESCE_MS = 100;

interface SavedWatch {
  expr: string;
  rateHz: number;
}

function describeRate(rateHz: number): string {
  return rateHz === 0 ? "off" : `${rateHz} Hz`;
}

/** Tree view of live expressions. A member is read only while it is showing. */
export class LiveWatch implements vscode.TreeDataProvider<WatchInfo> {
  private readonly changed = new vscode.EventEmitter<WatchInfo | undefined>();
  readonly onDidChangeTreeData = this.changed.event;

  private roots: WatchInfo[] = [];
  private readonly byId = new Map<number, WatchInfo>();
  private readonly children = new Map<number, WatchInfo[]>();
  private dirty = new Set<number>();
  private timer: NodeJS.Timeout | undefined;

  constructor(private readonly client: DataClient) {
    client.on("update", (values: WatchValue[]) => this.onUpdate(values));
  }

  private stateFile(): string | undefined {
    const folder = vscode.workspace.workspaceFolders?.[0];
    return folder && path.join(folder.uri.fsPath, ".jlinkd", "state.json");
  }

  private load(): SavedWatch[] {
    const file = this.stateFile();
    try {
      return file ? JSON.parse(fs.readFileSync(file, "utf8")).watches ?? [] : [];
    } catch {
      return []; // No saved state yet.
    }
  }

  private save(): void {
    const file = this.stateFile();
    if (!file) {
      return;
    }
    fs.mkdirSync(path.dirname(file), { recursive: true });
    const watches = this.roots.map(({ expr, rateHz }) => ({ expr, rateHz }));
    fs.writeFileSync(file, JSON.stringify({ watches }, null, 2) + "\n");
  }

  private onUpdate(values: WatchValue[]): void {
    for (const { id, value } of values) {
      const item = this.byId.get(id);
      if (item) {
        item.value = value;
        this.dirty.add(id);
      }
    }
    this.timer ??= setTimeout(() => this.flush(), UPDATE_COALESCE_MS);
  }

  private flush(): void {
    this.timer = undefined;
    for (const id of this.dirty) {
      this.changed.fire(this.byId.get(id));
    }
    this.dirty.clear();
  }

  /** Adopt what the backend has; a fresh backend gets the saved list. */
  async sync(): Promise<void> {
    let list = await this.client.call<WatchInfo[]>("watch.list");
    if (list.length === 0) {
      for (const saved of this.load()) {
        await this.client.call("watch.add", saved).catch(() => undefined);
      }
      list = await this.client.call<WatchInfo[]>("watch.list");
    }
    this.byId.clear();
    this.children.clear();
    this.roots = list.filter((item) => item.parent === null);
    for (const item of this.roots) {
      this.byId.set(item.id, item);
    }
    // Entries left open re-request their members from getChildren.
    this.changed.fire(undefined);
  }

  /** Add an expression, or find it if it is already being watched. */
  async add(expr: string): Promise<WatchInfo> {
    const existing = this.roots.find((root) => root.expr === expr);
    if (existing) {
      return existing;
    }
    const item = await this.client.call<WatchInfo>("watch.add", { expr });
    this.roots.push(item);
    this.byId.set(item.id, item);
    this.save();
    this.changed.fire(undefined);
    return item;
  }

  private forget(id: number): void {
    for (const child of this.children.get(id) ?? []) {
      this.forget(child.id);
      this.byId.delete(child.id);
    }
    this.children.delete(id);
  }

  async remove(item: WatchInfo): Promise<void> {
    await this.client.call("watch.remove", { id: item.id });
    this.forget(item.id);
    this.byId.delete(item.id);
    if (item.parent === null) {
      this.roots = this.roots.filter((root) => root.id !== item.id);
      this.save();
      this.changed.fire(undefined);
    } else {
      const parent = this.byId.get(item.parent);
      if (parent) {
        const siblings = this.children.get(parent.id) ?? [];
        this.children.set(
          parent.id,
          siblings.filter((sibling) => sibling.id !== item.id),
        );
        this.changed.fire(parent);
      }
    }
  }

  /** Stop reading the members of a collapsed entry. */
  async collapse(item: WatchInfo): Promise<void> {
    this.forget(item.id);
    await this.client.call("watch.collapse", { id: item.id });
    // VS Code only asks for children again after being told they changed.
    this.changed.fire(item);
  }

  private applyRate(item: WatchInfo, rateHz: number): void {
    item.rateHz = rateHz;
    for (const child of this.children.get(item.id) ?? []) {
      this.applyRate(child, rateHz);
    }
  }

  /** Set the rate for an entry and everything currently open under it. */
  async setRate(item: WatchInfo, rateHz: number): Promise<void> {
    await this.client.call("watch.setRate", { id: item.id, rateHz });
    this.applyRate(item, rateHz);
    if (item.parent === null) {
      this.save();
    }
    this.changed.fire(item);
  }

  async write(item: WatchInfo, value: string): Promise<void> {
    await this.client.call("watch.write", { id: item.id, value });
  }

  getTreeItem(item: WatchInfo): vscode.TreeItem {
    const state = item.expandable
      ? vscode.TreeItemCollapsibleState.Collapsed
      : vscode.TreeItemCollapsibleState.None;
    const tree = new vscode.TreeItem(item.name, state);
    // The expression is stable across restarts, so open entries stay open.
    tree.id = item.expr;
    tree.description = item.error ?? `${item.value}   [${describeRate(item.rateHz)}]`;
    tree.tooltip = item.error ?? `${item.expr}\n${item.type}\n${item.value}\nrefresh: ${describeRate(item.rateHz)}`;
    tree.contextValue = item.writable ? "watchWritable" : "watch";
    if (item.error) {
      tree.iconPath = new vscode.ThemeIcon("error", new vscode.ThemeColor("errorForeground"));
    }
    return tree;
  }

  async getChildren(item?: WatchInfo): Promise<WatchInfo[]> {
    if (!item) {
      return this.roots;
    }
    if (!this.children.has(item.id)) {
      const members = await this.client.call<WatchInfo[]>("watch.expand", { id: item.id });
      this.children.set(item.id, members);
      for (const member of members) {
        this.byId.set(member.id, member);
      }
    }
    return this.children.get(item.id) ?? [];
  }

  getParent(item: WatchInfo): WatchInfo | undefined {
    return item.parent === null ? undefined : this.byId.get(item.parent);
  }
}
