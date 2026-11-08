import uPlot from "uplot";
import "uplot/dist/uPlot.min.css";
import "./style.css";
import {
  Client,
  EVENT_FAULT,
  EVENT_HALTED,
  EVENT_RELOAD,
  EVENT_RESET,
  EVENT_RESUMED,
  type QueryResult,
  type SeriesInfo,
  type TimelineEvent,
} from "./client";

const NS = 1e9;
const REFRESH_MS = 33;
const STATUS_MS = 1000;
/** Seconds per division, in the order the wheel steps through them. */
const DIVISIONS = [0.001, 0.002, 0.005, 0.01, 0.02, 0.05, 0.1, 0.2, 0.5, 1, 2, 5, 10, 30];
const DIVISIONS_ON_SCREEN = 10;
/** Wheel travel for one step, so that a trackpad does not race through them. */
const WHEEL_STEP = 60;
/** Individual samples get a marker at this zoom and below. */
const MARKERS_AT_DIVISION = 0.02;
/** Newest data is held back slightly so the right edge is not ragged. */
const FOLLOW_LAG_S = 0.03;
const PALETTE = ["#4dabf7", "#ff922b", "#51cf66", "#f06595", "#ffd43b", "#b197fc", "#20c997", "#ff6b6b"];
const DEFAULT_GROUP = "default";

const byId = <T extends HTMLElement>(id: string) => document.getElementById(id) as T;

interface Halted {
  from: number;
  to: number | null;
}

interface GroupPlot {
  name: string;
  members: SeriesInfo[];
  plot: uPlot;
  element: HTMLElement;
}

const client = new Client();
let series: SeriesInfo[] = [];
const hidden = new Set<number>();
let groups: GroupPlot[] = [];
let followLive = true;
let division = 0.5;
/** Right edge of the view, in seconds since the backend started. */
let viewEnd = 0;
/** Server clock: seconds = serverNow + (performance.now() - serverNowAt) / 1000. */
let serverNow = 0;
let serverNowAt = performance.now();
let halts: Halted[] = [];
let markers: TimelineEvent[] = [];
let cursorA: number | null = null;
let cursorB: number | null = null;
let querying = false;
let lastQuery = 0;
let lastColumns: { result: QueryResult; xs: number[] } | null = null;
let recording = false;

const nowSeconds = () => serverNow + (performance.now() - serverNowAt) / 1000;

function colorOf(item: SeriesInfo, index: number): string {
  return item.color ?? PALETTE[index % PALETTE.length];
}

function withAlpha(color: string, alpha: string): string {
  return color.startsWith("#") && color.length === 7 ? color + alpha : color;
}

function applyEvent(event: TimelineEvent): void {
  const seconds = event.t / NS;
  if (event.kind === EVENT_HALTED) {
    halts.push({ from: seconds, to: null });
  } else if (event.kind === EVENT_RESUMED) {
    const open = halts[halts.length - 1];
    if (open && open.to === null) {
      open.to = seconds;
    }
  } else if (event.kind === EVENT_RESET || event.kind === EVENT_RELOAD || event.kind === EVENT_FAULT) {
    markers.push({ t: seconds, kind: event.kind });
  }
}

function drawOverlays(plot: uPlot): void {
  const context = plot.ctx;
  const { left, top, width, height } = plot.bbox;
  const min = plot.scales.x.min ?? 0;
  const max = plot.scales.x.max ?? 0;
  const toX = (value: number) => plot.valToPos(value, "x", true);
  context.save();
  context.beginPath();
  context.rect(left, top, width, height);
  context.clip();

  context.fillStyle = "rgba(255, 100, 100, 0.12)";
  for (const halt of halts) {
    const end = halt.to ?? max;
    if (end >= min && halt.from <= max) {
      const x0 = toX(Math.max(halt.from, min));
      context.fillRect(x0, top, toX(Math.min(end, max)) - x0, height);
    }
  }
  context.lineWidth = devicePixelRatio;
  const line = (value: number, style: string) => {
    if (value >= min && value <= max) {
      const x = Math.round(toX(value)) + 0.5;
      context.strokeStyle = style;
      context.beginPath();
      context.moveTo(x, top);
      context.lineTo(x, top + height);
      context.stroke();
    }
  };
  for (const marker of markers) {
    line(marker.t, marker.kind === EVENT_FAULT ? "#ff6b6b" : "#ffd43b");
  }
  if (cursorA !== null) line(cursorA, "#ffffff");
  if (cursorB !== null) line(cursorB, "#ffa8a8");
  context.restore();
}

const spanSeconds = () => division * DIVISIONS_ON_SCREEN;

function stepDivision(steps: number): void {
  const index = DIVISIONS.indexOf(division);
  division = DIVISIONS[Math.min(DIVISIONS.length - 1, Math.max(0, index + steps))];
  byId<HTMLSelectElement>("division").value = String(division);
}

/** Grid lines on exact multiples of the division. */
function divisionSplits(_plot: uPlot, _axis: number, min: number, max: number): number[] {
  const splits: number[] = [];
  const epsilon = division * 1e-6;
  for (let tick = Math.ceil((min - epsilon) / division); tick * division <= max + epsilon; tick++) {
    splits.push(tick * division);
  }
  return splits;
}

function divisionLabels(_plot: uPlot, splits: number[]): string[] {
  const decimals = division >= 1 ? 0 : Math.ceil(-Math.log10(division) - 1e-9);
  return splits.map((value) => value.toFixed(decimals));
}

function stopFollowing(): void {
  followLive = false;
  syncFollowButton();
}

function syncFollowButton(): void {
  const button = byId<HTMLButtonElement>("follow");
  button.textContent = followLive ? "Following" : "Paused";
  button.setAttribute("aria-pressed", String(followLive));
}

function attachInteractions(plot: uPlot): void {
  const over = plot.over;
  let drag: { x: number; end: number } | null = null;
  let moved = false;
  let wheelTravel = 0;

  over.addEventListener(
    "wheel",
    (event) => {
      event.preventDefault();
      wheelTravel += event.deltaY;
      if (Math.abs(wheelTravel) < WHEEL_STEP) return;
      const steps = Math.sign(wheelTravel);
      wheelTravel = 0;
      const rect = over.getBoundingClientRect();
      const fraction = (event.clientX - rect.left) / rect.width;
      const end = followLive ? nowSeconds() - FOLLOW_LAG_S : viewEnd;
      const pivot = end - spanSeconds() * (1 - fraction);
      stopFollowing();
      stepDivision(steps);
      viewEnd = pivot + spanSeconds() * (1 - fraction);
    },
    { passive: false },
  );
  over.addEventListener("mousedown", (event) => {
    drag = { x: event.clientX, end: followLive ? nowSeconds() - FOLLOW_LAG_S : viewEnd };
    moved = false;
  });
  window.addEventListener("mousemove", (event) => {
    if (!drag) return;
    const delta = event.clientX - drag.x;
    if (Math.abs(delta) > 3) moved = true;
    if (moved) {
      stopFollowing();
      viewEnd = drag.end - (delta / over.clientWidth) * spanSeconds();
    }
  });
  window.addEventListener("mouseup", (event) => {
    if (drag && !moved && event.target === over) {
      placeCursor(plot.posToVal(event.offsetX, "x"));
    }
    drag = null;
  });
}

function placeCursor(value: number): void {
  if (cursorA === null || cursorB !== null) {
    cursorA = value;
    cursorB = null;
  } else {
    cursorB = value;
  }
  updateMeasure();
}

function valueAt(item: SeriesInfo, seconds: number): number | null {
  if (!lastColumns) return null;
  const { result, xs } = lastColumns;
  const data = result.series.get(item.seriesId);
  if (!data || xs.length === 0) return null;
  const step = xs.length > 1 ? xs[1] - xs[0] : 1;
  const column = Math.min(xs.length - 1, Math.max(0, Math.round((seconds - xs[0]) / step)));
  // The nearest column that has a sample.
  for (let distance = 0; distance < xs.length; distance++) {
    for (const index of [column - distance, column + distance]) {
      if (index >= 0 && index < xs.length && !Number.isNaN(data.min[index])) {
        return (data.min[index] + data.max[index]) / 2;
      }
    }
  }
  return null;
}

function formatSeconds(seconds: number): string {
  const size = Math.abs(seconds);
  if (size >= 1) return `${seconds.toFixed(3)} s`;
  if (size >= 1e-3) return `${(seconds * 1e3).toFixed(3)} ms`;
  return `${(seconds * 1e6).toFixed(1)} µs`;
}

function updateMeasure(): void {
  const output = byId("measure");
  if (cursorA === null) {
    output.textContent = "Click a plot to place cursor A, then B.";
    return;
  }
  if (cursorB === null) {
    output.textContent = `A = ${cursorA.toFixed(4)} s. Click for B.`;
    return;
  }
  const first = series.find((item) => !hidden.has(item.seriesId));
  let text = `Δt = ${formatSeconds(cursorB - cursorA)}`;
  if (first) {
    const a = valueAt(first, cursorA);
    const b = valueAt(first, cursorB);
    if (a !== null && b !== null) {
      text += `  Δ${first.name} = ${(b - a).toPrecision(5)}`;
    }
  }
  output.textContent = text;
}

function destroyPlots(): void {
  for (const group of groups) {
    group.plot.destroy();
    group.element.remove();
  }
  groups = [];
}

function plotSize(element: HTMLElement): { width: number; height: number } {
  const container = byId("plots");
  const count = Math.max(1, groups.length + 1);
  return {
    width: element.clientWidth || container.clientWidth,
    height: Math.max(120, Math.floor(container.clientHeight / count) - 8),
  };
}

function buildGroup(name: string, members: SeriesInfo[], total: number): GroupPlot {
  const element = document.createElement("div");
  element.className = "group";
  const title = document.createElement("h2");
  title.textContent = name;
  element.append(title);
  byId("plots").append(element);

  const definitions: uPlot.Series[] = [{}];
  const bands: uPlot.Band[] = [];
  members.forEach((item) => {
    const color = colorOf(item, series.indexOf(item));
    const base = { stroke: color, width: 1, points: { show: false }, spanGaps: false } as const;
    definitions.push(
      {
        ...base,
        label: `${item.name} max`,
        points: { show: () => division <= MARKERS_AT_DIVISION, size: 5, fill: color, stroke: color },
      },
      { ...base, label: `${item.name} min` },
    );
    const index = definitions.length - 2;
    bands.push({ series: [index, index + 1], fill: withAlpha(color, "33") });
  });

  const height = Math.max(120, Math.floor(byId("plots").clientHeight / total) - 28);
  const plot = new uPlot(
    {
      width: element.clientWidth || 600,
      height,
      legend: { show: false },
      cursor: { drag: { x: false, y: false, setScale: false }, sync: { key: "timeline" } },
      scales: { x: { time: false, auto: false } },
      axes: [
        {
          stroke: "#9aa0a6",
          grid: { stroke: "#ffffff22" },
          ticks: { stroke: "#ffffff22" },
          splits: divisionSplits,
          values: divisionLabels,
        },
        { stroke: "#9aa0a6", grid: { stroke: "#ffffff14" }, ticks: { stroke: "#ffffff22" }, size: 64 },
      ],
      series: definitions,
      bands,
      hooks: { draw: [drawOverlays] },
    },
    [[], ...members.flatMap(() => [[], []])] as unknown as uPlot.AlignedData,
    element,
  );
  attachInteractions(plot);
  return { name, members, plot, element };
}

function rebuild(): void {
  destroyPlots();
  const visible = series.filter((item) => !hidden.has(item.seriesId) && item.actualMode !== "none");
  byId("empty").hidden = series.length > 0;
  const names = [...new Set(visible.map((item) => item.group ?? DEFAULT_GROUP))];
  for (const name of names) {
    const members = visible.filter((item) => (item.group ?? DEFAULT_GROUP) === name);
    groups.push(buildGroup(name, members, names.length));
  }
  renderSidebar();
}

function describeMode(item: SeriesInfo): string {
  if (!item.rateHz) return item.actualMode;
  const rate = item.rateHz >= 1000 ? `${item.rateHz / 1000} kHz` : `${item.rateHz} Hz`;
  return `${item.actualMode} ${rate}`;
}

function renderSidebar(): void {
  const list = byId("series");
  list.replaceChildren();
  series.forEach((item, index) => {
    const row = document.createElement("label");
    row.className = "series" + (item.error ? " broken" : "");
    row.title = item.error ?? item.root;

    const toggle = document.createElement("input");
    toggle.type = "checkbox";
    toggle.checked = !hidden.has(item.seriesId);
    toggle.style.accentColor = colorOf(item, index);
    toggle.onchange = () => {
      if (toggle.checked) hidden.delete(item.seriesId);
      else hidden.add(item.seriesId);
      rebuild();
    };

    const name = document.createElement("span");
    name.className = "name";
    name.textContent = item.name;

    const mode = document.createElement("span");
    mode.className = `mode ${item.actualMode}`;
    mode.textContent = item.error ? "error" : describeMode(item);

    const remove = document.createElement("button");
    remove.textContent = "×";
    remove.title = "Remove";
    remove.onclick = async (event) => {
      event.preventDefault();
      await client.call("timeline.remove", { seriesId: item.seriesId });
      await loadSeries();
    };
    row.append(toggle, name, mode, remove);
    list.append(row);
  });
}

async function loadSeries(): Promise<void> {
  const reply = await client.call<{ series: SeriesInfo[] }>("timeline.list");
  series = reply.series;
  rebuild();
}

/** Columns inside a halt are real gaps. Elsewhere an empty column only means
 *  samples are sparser than pixels, so the line continues across it. */
function isHalted(seconds: number): boolean {
  for (let index = halts.length - 1; index >= 0; index--) {
    const halt = halts[index];
    if (seconds >= halt.from && seconds <= (halt.to ?? Infinity)) return true;
    if (halt.to !== null && halt.to < seconds) return false;
  }
  return false;
}

function toPlotValues(data: Float64Array, gaps: boolean[]): (number | null | undefined)[] {
  const out = new Array<number | null | undefined>(data.length);
  for (let index = 0; index < data.length; index++) {
    const value = data[index];
    out[index] = Number.isNaN(value) ? (gaps[index] ? null : undefined) : value;
  }
  return out;
}

function draw(result: QueryResult): void {
  const step = (result.t1 - result.t0) / result.columns / NS;
  const start = result.t0 / NS;
  const xs = Array.from({ length: result.columns }, (_, index) => start + index * step);
  const gaps = xs.map(isHalted);
  lastColumns = { result, xs };
  byId("plots").dataset.range = `${result.t0},${result.t1}`;
  for (const group of groups) {
    const arrays: (number | null | undefined)[][] = [];
    for (const member of group.members) {
      const data = result.series.get(member.seriesId);
      if (data) {
        arrays.push(toPlotValues(data.max, gaps), toPlotValues(data.min, gaps));
      } else {
        const empty = new Array<null>(result.columns).fill(null);
        arrays.push(empty, empty);
      }
    }
    // Data first: setScale derives the visible index range from current data.
    group.plot.batch(() => {
      group.plot.setData([xs, ...arrays] as unknown as uPlot.AlignedData, false);
      group.plot.setScale("x", { min: start, max: result.t1 / NS });
    });
  }
}

async function refresh(): Promise<void> {
  querying = true;
  try {
    const ids = groups.flatMap((group) => group.members.map((member) => member.seriesId));
    const width = groups[0]?.plot.bbox.width ? Math.round(groups[0].plot.bbox.width / devicePixelRatio) : 0;
    if (ids.length > 0 && width > 0) {
      const end = followLive ? nowSeconds() - FOLLOW_LAG_S : viewEnd;
      if (followLive) viewEnd = end;
      const result = await client.call<QueryResult>("timeline.query", {
        seriesIds: ids,
        t0: Math.round((end - spanSeconds()) * NS),
        t1: Math.round(end * NS),
        widthPx: width,
      });
      draw(result);
    }
  } catch {
    // Reconnecting; the next frame tries again.
  } finally {
    querying = false;
  }
}

function frame(time: number): void {
  if (!querying && time - lastQuery >= REFRESH_MS) {
    lastQuery = time;
    void refresh();
  }
  requestAnimationFrame(frame);
}

interface Stats {
  hss: { records: number; lost: number };
}

async function refreshStatus(): Promise<void> {
  try {
    const status = await client.call<{ nowNs: number; connected: boolean; device?: string; speedKhz?: number }>(
      "session.status",
    );
    // A later reply replaces the estimate only if it is meaningfully different,
    // so the view does not jitter with message latency.
    const estimate = nowSeconds();
    const reported = status.nowNs / NS;
    if (Math.abs(estimate - reported) > 0.02 || serverNow === 0) {
      serverNow = reported;
      serverNowAt = performance.now();
    }
    const stats = await client.call<Stats>("session.stats");
    byId("connection").textContent = status.connected
      ? `${status.device} @ ${(status.speedKhz ?? 0) / 1000} MHz`
      : "No target connected";
    const hssCount = series.filter((item) => item.actualMode === "hss").length;
    const rate = series.find((item) => item.actualMode === "hss")?.rateHz ?? 0;
    byId("rates").textContent =
      hssCount > 0 ? `HSS ${hssCount} signals @ ${rate} Hz, lost ${stats.hss.lost}` : "";
  } catch {
    byId("connection").textContent = "Reconnecting…";
  }
}

interface Rate {
  rateHz: number;
  maxHz: number;
}

function showRate(rate: Rate): void {
  const select = byId<HTMLSelectElement>("rate");
  const value = String(rate.rateHz);
  if (![...select.options].some((option) => option.value === value)) {
    select.append(new Option(`${value} Hz`, value));
  }
  select.value = value;
  select.classList.toggle("off", rate.rateHz === 0);
}

async function changeRate(rateHz: number): Promise<void> {
  try {
    showRate(await client.call<Rate>("timeline.setRate", { rateHz }));
    await loadSeries();
  } catch (error) {
    byId("measure").textContent = String(error);
    showRate(await client.call<Rate>("timeline.rate"));
  }
}

async function start(): Promise<void> {
  const reply = await client.call<{ events: TimelineEvent[] }>("events.query", {});
  halts = [];
  markers = [];
  reply.events.forEach(applyEvent);
  await refreshStatus();
  showRate(await client.call<Rate>("timeline.rate"));
  await loadSeries();
}

client.onEvent = (event) => {
  applyEvent(event);
  // Sampling changes follow adding, removing or rebinding series.
  if (event.kind !== EVENT_HALTED && event.kind !== EVENT_RESUMED) {
    void loadSeries();
  }
};
client.onConnection = (connected) => {
  if (connected) {
    void start();
  } else {
    byId("connection").textContent = "Reconnecting…";
  }
};

byId<HTMLFormElement>("add").addEventListener("submit", async (event) => {
  event.preventDefault();
  const input = byId<HTMLInputElement>("expr");
  const group = byId<HTMLInputElement>("group").value.trim();
  try {
    await client.call("timeline.add", {
      expr: input.value.trim(),
      mode: byId<HTMLSelectElement>("mode").value,
      ...(group ? { group } : {}),
    });
    input.value = "";
    await loadSeries();
  } catch (error) {
    byId("measure").textContent = String(error);
  }
});
byId<HTMLSelectElement>("rate").addEventListener("change", (event) => {
  void changeRate(Number((event.target as HTMLSelectElement).value));
});
byId<HTMLSelectElement>("division").addEventListener("change", (event) => {
  division = Number((event.target as HTMLSelectElement).value);
});
byId("follow").addEventListener("click", () => {
  followLive = !followLive;
  syncFollowButton();
});
byId("record").addEventListener("click", async () => {
  const button = byId<HTMLButtonElement>("record");
  try {
    if (recording) {
      const result = await client.call<{ path: string; samples: number }>("record.stop");
      byId("measure").textContent = `Saved ${result.samples} samples to ${result.path}`;
    } else {
      await client.call("record.start");
    }
    recording = !recording;
    button.textContent = recording ? "Stop" : "Record";
    button.classList.toggle("active", recording);
  } catch (error) {
    byId("measure").textContent = String(error);
  }
});
byId("export").addEventListener("click", async () => {
  const end = followLive ? nowSeconds() : viewEnd;
  try {
    const result = await client.call<{ path: string; rows: number }>("record.export", {
      format: "csv",
      seriesIds: series.map((item) => item.seriesId),
      t0: Math.round((end - spanSeconds()) * NS),
      t1: Math.round(end * NS),
    });
    byId("measure").textContent = `Wrote ${result.rows} rows to ${result.path}`;
  } catch (error) {
    byId("measure").textContent = String(error);
  }
});
new ResizeObserver(() => {
  for (const group of groups) {
    const { width } = plotSize(group.element);
    group.plot.setSize({ width, height: group.plot.height });
  }
}).observe(byId("plots"));

client.connect();
setInterval(() => void refreshStatus(), STATUS_MS);
requestAnimationFrame(frame);
