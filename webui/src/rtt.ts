import "./style.css";
import { Client } from "./client";
import { ThemeManager } from "./theme";

new ThemeManager();

const REFRESH_MS = 500;
const MAX_TERMINAL_CHARS = 200_000;

interface ChannelInfo {
  index: number;
  name: string;
  direction: string;
  size: number;
  decoder: string;
  bytes: number;
  frames: number;
  lost: number;
  resyncs: number;
}

interface SignalInfo {
  name: string;
  value: number | null;
  seriesId: number | null;
}

interface RttState {
  state: string;
  error: string | null;
}

const byId = <T extends HTMLElement>(id: string) => document.getElementById(id) as T;
const client = new Client();
let selected: number | null = null;

/** Terminal escape sequences are for real terminals; drop them here. */
function stripAnsi(text: string): string {
  // eslint-disable-next-line no-control-regex
  return text.replace(/\x1b\[[0-9;]*[A-Za-z]/g, "");
}

client.onRtt = (channel, data) => {
  if (channel !== selected) return;
  const terminal = byId("terminal");
  const atBottom = terminal.scrollTop + terminal.clientHeight >= terminal.scrollHeight - 8;
  const text = (terminal.textContent ?? "") + stripAnsi(new TextDecoder().decode(data));
  terminal.textContent = text.slice(-MAX_TERMINAL_CHARS);
  if (atBottom) terminal.scrollTop = terminal.scrollHeight;
};

async function select(channel: number): Promise<void> {
  if (selected !== null) {
    await client.call("rtt.unsubscribe", { ch: selected }).catch(() => undefined);
  }
  selected = channel;
  byId("terminal").textContent = "";
  await client.call("rtt.subscribe", { ch: channel });
}

function cell(row: HTMLTableRowElement, text: string, numeric = false): HTMLTableCellElement {
  const item = row.insertCell();
  item.textContent = text;
  if (numeric) item.className = "number";
  return item;
}

function header(table: HTMLTableElement, names: string[]): void {
  table.replaceChildren();
  const row = table.createTHead().insertRow();
  for (const name of names) {
    const heading = document.createElement("th");
    heading.textContent = name;
    row.append(heading);
  }
}

function renderChannels(channels: ChannelInfo[]): void {
  const table = byId<HTMLTableElement>("channels");
  header(table, ["#", "Name", "Dir", "Size", "Decoder", "Bytes", "Frames", "Lost", "Resyncs"]);
  const body = table.createTBody();
  for (const channel of channels) {
    if (channel.size === 0) continue;
    const row = body.insertRow();
    cell(row, String(channel.index), true);
    cell(row, channel.name);
    cell(row, channel.direction);
    cell(row, String(channel.size), true);
    cell(row, channel.decoder);
    cell(row, channel.direction === "up" ? String(channel.bytes) : "", true);
    cell(row, channel.direction === "up" ? String(channel.frames) : "", true);
    cell(row, channel.direction === "up" ? String(channel.lost) : "", true);
    cell(row, channel.direction === "up" ? String(channel.resyncs) : "", true);
  }

  const picker = byId<HTMLSelectElement>("channel");
  const up = channels.filter((channel) => channel.direction === "up" && channel.size > 0);
  const options = up.map((channel) => `${channel.index}:${channel.name}`).join();
  if (picker.dataset.options !== options) {
    picker.dataset.options = options;
    picker.replaceChildren(
      ...up.map((channel) => new Option(`${channel.index}: ${channel.name}`, String(channel.index))),
    );
    if (selected === null && up.length > 0) {
      void select(up[0].index);
    } else if (selected !== null) {
      picker.value = String(selected);
    }
  }
}

function renderSignals(signals: SignalInfo[]): void {
  const table = byId<HTMLTableElement>("signals");
  header(table, ["Signal", "Value", "Timeline"]);
  const body = table.createTBody();
  for (const signal of signals) {
    const row = body.insertRow();
    cell(row, signal.name);
    cell(row, signal.value === null ? "" : signal.value.toPrecision(6), true);
    const action = row.insertCell();
    const button = document.createElement("button");
    button.textContent = signal.seriesId === null ? "Add" : "Remove";
    button.onclick = () =>
      void (signal.seriesId === null
        ? client.call("timeline.add", { rttSignal: signal.name })
        : client.call("timeline.remove", { seriesId: signal.seriesId })
      ).then(refresh);
    action.append(button);
  }
}

async function refresh(): Promise<void> {
  try {
    const status = await client.call<RttState>("rtt.status");
    byId("state").textContent = status.error ? `RTT: ${status.error}` : `RTT ${status.state}`;
    renderChannels((await client.call<{ channels: ChannelInfo[] }>("rtt.channels")).channels);
    renderSignals((await client.call<{ signals: SignalInfo[] }>("rtt.signals")).signals);
  } catch {
    byId("state").textContent = "Reconnecting…";
  }
}

byId<HTMLSelectElement>("channel").addEventListener("change", (event) => {
  void select(Number((event.target as HTMLSelectElement).value));
});
byId<HTMLFormElement>("send").addEventListener("submit", async (event) => {
  event.preventDefault();
  const input = byId<HTMLInputElement>("input");
  await client.call("rtt.write", { ch: selected ?? 0, data: `${input.value}\n` }).catch(() => undefined);
  input.value = "";
});
client.onConnection = (connected) => {
  if (connected) {
    const channel = selected;
    selected = null;
    if (channel !== null) void select(channel);
    void refresh();
  }
};

client.connect();
setInterval(() => void refresh(), REFRESH_MS);
