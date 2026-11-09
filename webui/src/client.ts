/** Connection to the backend's data channel (DESIGN §6.2). */

export interface SeriesInfo {
  seriesId: number;
  name: string;
  root: string;
  actualMode: string;
  rateHz: number;
  error: string | null;
  group: string | null;
  color: string | null;
}

export interface TimelineEvent {
  t: number;
  kind: number;
  detail?: unknown;
}

export interface QueryResult {
  t0: number;
  t1: number;
  columns: number;
  series: Map<number, { min: Float64Array; max: Float64Array }>;
}

export const EVENT_HALTED = 1;
export const EVENT_RESUMED = 2;
export const EVENT_RESET = 3;
export const EVENT_RELOAD = 4;
export const EVENT_SAMPLING = 5;
export const EVENT_FAULT = 6;

const KIND_RTT = 2;
const KIND_EVENT = 3;
const KIND_QUERY = 4;
const RECONNECT_MS = 1000;

interface Pending {
  resolve: (value: unknown) => void;
  reject: (reason: Error) => void;
}

export class Client {
  private socket: WebSocket | null = null;
  private nextId = 1;
  private readonly pending = new Map<number, Pending>();

  onEvent: (event: TimelineEvent) => void = () => {};
  onRtt: (channel: number, data: Uint8Array) => void = () => {};
  onConnection: (connected: boolean) => void = () => {};

  connect(): void {
    const socket = new WebSocket(`ws://${location.host}/ws`);
    socket.binaryType = "arraybuffer";
    socket.onopen = () => {
      this.socket = socket;
      this.onConnection(true);
    };
    socket.onclose = () => {
      this.socket = null;
      for (const request of this.pending.values()) {
        request.reject(new Error("connection lost"));
      }
      this.pending.clear();
      this.onConnection(false);
      setTimeout(() => this.connect(), RECONNECT_MS);
    };
    socket.onmessage = (message) => {
      if (typeof message.data === "string") {
        this.onText(message.data);
      } else {
        this.onBinary(message.data as ArrayBuffer);
      }
    };
  }

  private onText(text: string): void {
    const message = JSON.parse(text);
    if (message.id === undefined || message.id === null) {
      return; // A notification this page does not use.
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

  private onBinary(buffer: ArrayBuffer): void {
    const view = new DataView(buffer);
    const kind = view.getUint8(0);
    if (kind === KIND_RTT) {
      this.onRtt(view.getUint8(4), new Uint8Array(buffer, 9, view.getUint32(5, true)));
    } else if (kind === KIND_EVENT) {
      let detail: unknown = undefined;
      if (buffer.byteLength >= 16) {
        const len = view.getUint16(14, true);
        if (len > 0 && buffer.byteLength >= 16 + len) {
          try {
            detail = JSON.parse(new TextDecoder().decode(new Uint8Array(buffer, 16, len)));
          } catch {
            // Ignore malformed event detail JSON
          }
        }
      }
      this.onEvent({ t: Number(view.getBigInt64(4, true)), kind: view.getUint16(12, true), detail });
    } else if (kind === KIND_QUERY) {
      const id = view.getUint32(4, true);
      const request = this.pending.get(id);
      if (request) {
        this.pending.delete(id);
        request.resolve(parseQuery(buffer, view));
      }
    }
  }

  call<T>(method: string, params: object = {}): Promise<T> {
    if (!this.socket) {
      return Promise.reject(new Error("not connected"));
    }
    const id = this.nextId++;
    this.socket.send(JSON.stringify({ jsonrpc: "2.0", id, method, params }));
    return new Promise<T>((resolve, reject) => {
      this.pending.set(id, { resolve: resolve as (value: unknown) => void, reject });
    });
  }
}

function parseQuery(buffer: ArrayBuffer, view: DataView): QueryResult {
  const count = view.getUint16(2, true);
  const columns = view.getUint32(8, true);
  const series = new Map<number, { min: Float64Array; max: Float64Array }>();
  let offset = 32;
  for (let index = 0; index < count; index++) {
    const id = view.getUint32(offset, true);
    offset += 8;
    const min = new Float64Array(buffer, offset, columns);
    offset += columns * 8;
    const max = new Float64Array(buffer, offset, columns);
    offset += columns * 8;
    series.set(id, { min, max });
  }
  return {
    t0: Number(view.getBigInt64(16, true)),
    t1: Number(view.getBigInt64(24, true)),
    columns,
    series,
  };
}
