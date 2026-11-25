import * as vscode from "vscode";
import type { DiagnosticsManager } from "./errors";

const DEBUG_TYPE = "haltd";

function activeSession(): vscode.DebugSession | undefined {
  const session = vscode.debug.activeDebugSession;
  return session?.type === DEBUG_TYPE ? session : undefined;
}

/** Failures that only mean there is nothing to show right now: the session is
 *  ending, or the target is running. */
const EXPECTED_FAILURES = ["No debugger available", "the target is running"];

/** Ask the adapter for view data. Undefined when there is nothing to show. */
async function fetchFor<T>(command: string, args?: object): Promise<T | undefined> {
  const session = activeSession();
  if (!session) {
    return undefined;
  }
  try {
    return (await session.customRequest(command, args)) as T;
  } catch (error) {
    if (EXPECTED_FAILURES.some((text) => String(error).includes(text))) {
      return undefined;
    }
    throw error;
  }
}

interface PeripheralInfo {
  kind: "peripheral";
  name: string;
  description: string;
  base: string;
}

interface FieldInfo {
  kind: "field";
  peripheral: string;
  register: string;
  name: string;
  description: string;
  bits: string;
  value: string | null;
  writable: boolean;
}

interface RegisterInfo {
  kind: "register";
  peripheral: string;
  name: string;
  description: string;
  address: string;
  value: string;
  known: boolean;
  sideEffects: boolean;
  writable: boolean;
  fields: FieldInfo[];
}

export type PeripheralNode = PeripheralInfo | RegisterInfo | FieldInfo;

type RawRegister = Omit<RegisterInfo, "kind" | "peripheral" | "fields"> & {
  fields: Omit<FieldInfo, "kind" | "peripheral" | "register">[];
};

/** Peripheral → register → field. A peripheral is only read while open. */
export class PeripheralsView implements vscode.TreeDataProvider<PeripheralNode> {
  private readonly changed = new vscode.EventEmitter<PeripheralNode | undefined>();
  readonly onDidChangeTreeData = this.changed.event;
  /** Registers read on request, kept so a refresh does not read them again. */
  private readonly forced = new Map<string, string>();

  refresh(): void {
    this.changed.fire(undefined);
  }

  clear(): void {
    this.forced.clear();
    this.refresh();
  }

  private async loadRegisters(peripheral: string, force?: string): Promise<RegisterInfo[]> {
    const reply = await fetchFor<{ registers: RawRegister[] }>("haltd/peripheralRead", { peripheral, force });
    return (reply?.registers ?? []).map((register) => ({
      ...register,
      kind: "register",
      peripheral,
      fields: register.fields.map((field) => ({
        ...field,
        kind: "field",
        peripheral,
        register: register.name,
      })),
    }));
  }

  async getChildren(node?: PeripheralNode): Promise<PeripheralNode[]> {
    if (!node) {
      const reply = await fetchFor<{ peripherals: Omit<PeripheralInfo, "kind">[] }>("haltd/peripherals");
      return (reply?.peripherals ?? []).map((peripheral) => ({ ...peripheral, kind: "peripheral" }));
    }
    if (node.kind === "peripheral") {
      const registers = await this.loadRegisters(node.name);
      return registers.map((register) => {
        const value = this.forced.get(`${node.name}.${register.name}`);
        return value !== undefined ? { ...register, value } : register;
      });
    }
    return node.kind === "register" ? node.fields : [];
  }

  getTreeItem(node: PeripheralNode): vscode.TreeItem {
    switch (node.kind) {
      case "peripheral": {
        const item = new vscode.TreeItem(node.name, vscode.TreeItemCollapsibleState.Collapsed);
        item.id = node.name;
        item.description = node.base;
        item.tooltip = node.description;
        item.contextValue = "peripheral";
        return item;
      }
      case "register": {
        const expandable = node.known && node.fields.length > 0;
        const item = new vscode.TreeItem(
          node.name,
          expandable ? vscode.TreeItemCollapsibleState.Collapsed : vscode.TreeItemCollapsibleState.None,
        );
        item.id = `${node.peripheral}.${node.name}`;
        item.description = node.value;
        item.tooltip = `${node.address}\n${node.description}`;
        item.contextValue = `register${node.sideEffects ? "-sideEffects" : ""}${node.writable ? "-writable" : ""}`;
        return item;
      }
      case "field": {
        const item = new vscode.TreeItem(node.name, vscode.TreeItemCollapsibleState.None);
        item.id = `${node.peripheral}.${node.register}.${node.name}`;
        item.description = `${node.value ?? ""}   ${node.bits}`;
        item.tooltip = node.description;
        item.contextValue = `field${node.writable ? "-writable" : ""}`;
        return item;
      }
    }
  }

  /** Read one register that is normally left alone because reading changes it. */
  async readRegister(node: RegisterInfo): Promise<void> {
    const registers = await this.loadRegisters(node.peripheral, node.name);
    const fresh = registers.find((register) => register.name === node.name);
    if (fresh) {
      this.forced.set(`${node.peripheral}.${node.name}`, fresh.value);
      this.refresh();
    }
  }

  async write(node: RegisterInfo | FieldInfo): Promise<void> {
    const session = activeSession();
    if (!session) {
      return;
    }
    const current = node.value ?? "";
    const value = await vscode.window.showInputBox({ prompt: `New value for ${node.name}`, value: current.split(" ")[0] });
    if (value === undefined) {
      return;
    }
    await session.customRequest("haltd/peripheralWrite", {
      peripheral: node.peripheral,
      register: node.kind === "register" ? node.name : node.register,
      field: node.kind === "field" ? node.name : undefined,
      value,
    });
    this.refresh();
  }
}

interface TaskInfo {
  kind: "task";
  id: number;
  name: string;
  state: string;
  priority: number;
  stackFreeBytes: number;
}

interface FrameInfo {
  kind: "frame";
  task: number;
  index: number;
  name: string;
  line: number;
  source?: { path?: string };
}

export type TaskNode = TaskInfo | FrameInfo;

const STATE_ICONS: Record<string, string> = {
  running: "play",
  ready: "circle-outline",
  blocked: "watch",
  suspended: "debug-pause",
  deleted: "trash",
};

/** FreeRTOS tasks. Opening a task shows its call stack without switching to it. */
export class TasksView implements vscode.TreeDataProvider<TaskNode> {
  private readonly changed = new vscode.EventEmitter<TaskNode | undefined>();
  readonly onDidChangeTreeData = this.changed.event;

  refresh(): void {
    this.changed.fire(undefined);
  }

  async getChildren(node?: TaskNode): Promise<TaskNode[]> {
    if (!node) {
      const reply = await fetchFor<{ tasks: Omit<TaskInfo, "kind">[] }>("haltd/rtosTasks");
      return (reply?.tasks ?? []).map((task) => ({ ...task, kind: "task" }));
    }
    if (node.kind === "frame") {
      return [];
    }
    const reply = await fetchFor<{ stackFrames: Omit<FrameInfo, "kind" | "task" | "index">[] }>("stackTrace", {
      threadId: node.id,
    });
    return (reply?.stackFrames ?? []).map((frame, index) => ({ ...frame, kind: "frame", task: node.id, index }));
  }

  getTreeItem(node: TaskNode): vscode.TreeItem {
    if (node.kind === "task") {
      const item = new vscode.TreeItem(node.name, vscode.TreeItemCollapsibleState.Collapsed);
      item.id = `task-${node.id}`; // FreeRTOS allows tasks to share a name.
      item.description = `${node.state} · prio ${node.priority} · ${node.stackFreeBytes} B free`;
      item.iconPath = new vscode.ThemeIcon(STATE_ICONS[node.state] ?? "circle-outline");
      return item;
    }
    const item = new vscode.TreeItem(node.name, vscode.TreeItemCollapsibleState.None);
    item.id = `task-${node.task}-frame-${node.index}`;
    const path = node.source?.path;
    if (path && node.line > 0) {
      item.description = `${path.split(/[/\\]/).pop()}:${node.line}`;
      const line = Math.max(node.line - 1, 0);
      item.command = {
        command: "vscode.open",
        title: "Open",
        arguments: [vscode.Uri.file(path), { selection: new vscode.Range(line, 0, line, 0) }],
      };
    }
    return item;
  }
}

interface FaultEvent {
  summary: string;
  file?: string;
  line?: number;
  note?: string;
}

/** Tell the user why the target faulted. The editor has already opened on the
 *  faulting line, because the adapter marks the handler frames as uninteresting. */
export async function showFault(fault: FaultEvent, diagnostics: DiagnosticsManager): Promise<void> {
  diagnostics.recordFault(fault);
  const place = fault.file ? ` (${fault.file.split("/").pop()}:${fault.line})` : "";
  const message = `${fault.summary}${place}`;
  const choice = await vscode.window.showErrorMessage(`Haltd: ${message}`, "Details");
  if (choice === "Details") {
    diagnostics.outputChannel.show(true);
  }
}

interface CycleEvent {
  cycles: number;
  microseconds: number | null;
  wrapped: boolean;
}

/** Cycles the core ran between the last two stops. */
export function describeCycles(event: CycleEvent): string {
  const count = event.cycles.toLocaleString("en-US");
  const prefix = event.wrapped ? "wrapped, last " : "";
  if (event.microseconds === null) {
    return `$(watch) ${prefix}${count} cyc`;
  }
  const time =
    event.microseconds >= 1000 ? `${(event.microseconds / 1000).toFixed(2)} ms` : `${event.microseconds.toFixed(1)} µs`;
  return `$(watch) ${prefix}${count} cyc · ${time}`;
}
