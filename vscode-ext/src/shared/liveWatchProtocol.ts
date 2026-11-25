export type WatchColumnKey = "type" | "address" | "rate";

export type WatchFormat = "default" | "dec" | "uint" | "hex" | "bin" | "float" | "char";

export type WatchScalarKind = "bool" | "float" | "signed" | "unsigned" | "enum" | "pointer";

export type WatchpointAccess = "write" | "readWrite" | "read";

export type TargetRunState = "disconnected" | "running" | "halted";

export type ColumnWidths = Record<"name" | "value" | WatchColumnKey, number>;

export const ALL_OPTIONAL_COLUMNS: readonly WatchColumnKey[] = ["type", "address", "rate"];

export const DEFAULT_COLUMN_WIDTHS: ColumnWidths = {
  name: 220,
  value: 190,
  type: 130,
  address: 112,
  rate: 66,
};

export interface SymbolCompletionItem {
  name: string;
  type: string;
  address: string;
}

export interface WatchRowNode {
  id: number;
  parent: number | null;
  key: string;
  depth: number;
  name: string;
  expr: string;
  type: string;
  expandable: boolean;
  expanded: boolean;
  writable: boolean;
  rateHz: number;
  value: string;
  rawHex: string | null;
  scalarKind: WatchScalarKind | null;
  size: number | null;
  bitRange: string | null;
  address: string | null;
  error: string | null;
  plottable: boolean;
  format: WatchFormat;
  prevValue: string | null;
  prevRawHex: string | null;
  lastChangedAtMs: number | null;
  elapsedMs: number | null;
  changedOnStop: boolean;
  watchpoint: WatchpointAccess | null;
}

export interface WatchValuePatch {
  id: number;
  value: string;
  rawHex: string | null;
  address: string | null;
  error: string | null;
  prevValue: string | null;
  prevRawHex: string | null;
  lastChangedAtMs: number | null;
  elapsedMs: number | null;
  changedOnStop: boolean;
}

export type HostToWebviewMessage =
  | {
      type: "snapshot";
      rows: WatchRowNode[];
      columns: WatchColumnKey[];
      widths: ColumnWidths;
      freshnessMs: number;
      runState: TargetRunState;
      connected: boolean;
    }
  | {
      type: "patch";
      updates: WatchValuePatch[];
      runState: TargetRunState;
      connected: boolean;
    }
  | {
      type: "metaPatch";
      rows: WatchRowNode[];
      runState: TargetRunState;
      connected: boolean;
    }
  | {
      type: "state";
      runState: TargetRunState;
      connected: boolean;
      freshnessMs: number;
    }
  | {
      type: "columns";
      columns: WatchColumnKey[];
      widths: ColumnWidths;
    }
  | {
      type: "columnWidths";
      widths: ColumnWidths;
    }
  | {
      type: "revealRow";
      id: number;
    }
  | {
      type: "focusAddInput";
    }
  | {
      type: "completions";
      requestId: number;
      items: SymbolCompletionItem[];
    }
  | {
      type: "actionError";
      message: string;
    };

export type WebviewToHostMessage =
  | { type: "ready" }
  | { type: "add"; expr: string }
  | { type: "modify"; id: number; expr: string }
  | { type: "remove"; ids: number[] }
  | { type: "write"; id: number; value: string }
  | { type: "expand"; id: number }
  | { type: "collapse"; id: number }
  | { type: "setRate"; ids: number[]; rateHz: number }
  | { type: "setFormat"; id: number; format: WatchFormat }
  | { type: "reorder"; rootIds: number[] }
  | { type: "setColumns"; columns: WatchColumnKey[] }
  | { type: "setColumnWidths"; widths: ColumnWidths }
  | { type: "complete"; requestId: number; query: string }
  | { type: "addToTimeline"; exprs: string[] }
  | { type: "viewMemory"; address: string; name: string }
  | { type: "toggleWatchpoint"; id: number; access: WatchpointAccess | null }
  | { type: "copy"; text: string }
  | { type: "popOut" }
  | { type: "dockToPanel" };
