import { EventEmitter } from "events";
import WebSocket from "ws";

export const BACKEND_PORT = 47110;
const RECONNECT_MS = 1000;
const KIND_RTT = 2;

export interface WatchInfo {
  id: number;
  parent: number | null;
  name: string;
  expr: string;
  type: string;
  expandable: boolean;
  writable: boolean;
  rateHz: number;
  value: string;
  error: string | null;
  plottable: boolean;
  address: string | null;
  bitRange: string | null;
  size: number | null;
  rawHex: string | null;
  scalarKind: "bool" | "float" | "signed" | "unsigned" | "enum" | "pointer" | null;
}

export interface WatchValue {
  id: number;
  value: string;
  rawHex: string | null;
  address: string | null;
  error: string | null;
}

export interface HaltdDiagnostic {
  id: number;
  code: string;
  message: string;
  detail?: string;
  causes: string[];
  actions: string[];
}

export class BackendRpcError extends Error {
  constructor(
    message: string,
    readonly rpcCode: number,
    readonly haltd?: HaltdDiagnostic,
  ) {
    super(message);
    this.name = "BackendRpcError";
  }
}

export interface WsCloseInfo {
  code: number;
  reason: string;
  error?: string;
  expected: boolean;
}

interface Pending {
  resolve: (value: unknown) => void;
  reject: (reason: Error) => void;
}

/** JSON-RPC over the backend's WebSocket. Emits "open", "close", "update" and "rtt". */
export class DataClient extends EventEmitter {
  private socket: WebSocket | undefined;
  private nextId = 1;
  private readonly pending = new Map<number, Pending>();
  private wanted = false;
  private timer: NodeJS.Timeout | undefined;

  get connected(): boolean {
    return this.socket?.readyState === WebSocket.OPEN;
  }

  start(): void {
    this.wanted = true;
    if (!this.socket) {
      this.open();
    }
  }

  stop(): void {
    this.wanted = false;
    clearTimeout(this.timer);
    this.socket?.close();
  }

  private open(): void {
    const socket = new WebSocket(`ws://127.0.0.1:${BACKEND_PORT}/ws`);
    this.socket = socket;
    let lastSocketError: string | undefined;
    socket.on("open", () => this.emit("open"));
    socket.on("message", (data, isBinary) => {
      if (!isBinary) {
        this.onText(data.toString());
      } else {
        this.onBinary(data as Buffer);
      }
    });
    // "close" always follows, so capture the underlying error for diagnostics.
    socket.on("error", (err) => {
      lastSocketError = err instanceof Error ? err.message : String(err);
    });
    socket.on("close", (code, reason) => {
      this.socket = undefined;
      for (const request of this.pending.values()) {
        request.reject(new Error("connection lost"));
      }
      this.pending.clear();
      const closeInfo: WsCloseInfo = {
        code,
        reason: reason.toString("utf8"),
        error: lastSocketError,
        expected: !this.wanted,
      };
      this.emit("close", closeInfo);
      if (this.wanted) {
        this.timer = setTimeout(() => this.open(), RECONNECT_MS);
      }
    });
  }

  /** Only terminal bytes are of interest here; plots live in the web page. */
  private onBinary(frame: Buffer): void {
    if (frame[0] === KIND_RTT) {
      this.emit("rtt", frame[4], frame.subarray(9, 9 + frame.readUInt32LE(5)));
    }
  }

  private onText(text: string): void {
    const message = JSON.parse(text);
    if (message.method === "watch.update") {
      this.emit("update", message.params.values as WatchValue[]);
      return;
    }
    const request = this.pending.get(message.id);
    if (!request) {
      return;
    }
    this.pending.delete(message.id);
    if (message.error) {
      const rpcCode = typeof message.error.code === "number" ? message.error.code : -32000;
      const haltd = message.error.data as HaltdDiagnostic | undefined;
      request.reject(new BackendRpcError(String(message.error.message ?? "RPC error"), rpcCode, haltd));
    } else {
      request.resolve(message.result);
    }
  }

  call<T>(method: string, params: object = {}): Promise<T> {
    if (!this.connected) {
      return Promise.reject(new Error("the haltd backend is not connected"));
    }
    const id = this.nextId++;
    this.socket!.send(JSON.stringify({ jsonrpc: "2.0", id, method, params }));
    return new Promise<T>((resolve, reject) => {
      this.pending.set(id, { resolve: resolve as (value: unknown) => void, reject });
    });
  }
}
