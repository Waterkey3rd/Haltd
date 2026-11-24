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
}

export interface WatchValue {
  id: number;
  value: string;
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
    socket.on("open", () => this.emit("open"));
    socket.on("message", (data, isBinary) => {
      if (!isBinary) {
        this.onText(data.toString());
      } else {
        this.onBinary(data as Buffer);
      }
    });
    // "close" always follows, so errors need no handling beyond not throwing.
    socket.on("error", () => undefined);
    socket.on("close", () => {
      this.socket = undefined;
      for (const request of this.pending.values()) {
        request.reject(new Error("connection lost"));
      }
      this.pending.clear();
      this.emit("close");
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
      request.reject(new Error(message.error.message));
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
