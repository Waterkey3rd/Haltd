import * as fs from "fs";
import * as os from "os";
import * as path from "path";
import * as vscode from "vscode";
import { BACKEND_PORT, DataClient, type WatchInfo } from "./dataClient";
import {
  describeCycles,
  PeripheralsView,
  showFault,
  TasksView,
  type PeripheralNode,
} from "./debugViews";
import { registerDeviceAssist } from "./deviceAssist";
import { LiveWatch } from "./liveWatch";

const DEBUG_TYPE = "jlinkd";
const STATUS_MS = 1000;
const RATES = [
  { label: "Off", description: "refresh only when the target stops", rateHz: 0 },
  { label: "1 Hz", rateHz: 1 },
  { label: "2 Hz", rateHz: 2 },
  { label: "3 Hz", rateHz: 3 },
  { label: "4 Hz", rateHz: 4 },
  { label: "5 Hz", rateHz: 5 },
];
const RTT_TERMINAL_CHANNEL = 0;
const BUILT_IN_WATCH_VIEW = "workbench.debug.watchExpressionsView";
const BUILT_IN_ADD_TO_WATCH = ["debug.addToWatchExpressions", "editor.debug.action.selectionToWatch"];
const BACKEND_URL = `http://127.0.0.1:${BACKEND_PORT}/timeline`;
const RTT_URL = `http://127.0.0.1:${BACKEND_PORT}/rtt`;

/** The configured path, then the copy packaged with the extension, then
 *  ~/.cargo/bin, which VS Code started from a desktop launcher often lacks on PATH. */
export function adapterPath(extensionPath: string): string {
  const configured = vscode.workspace.getConfiguration("jlinkd").get<string>("path", "jlinkd");
  if (configured !== "jlinkd") {
    return configured;
  }
  const executable = process.platform === "win32" ? "jlinkd.exe" : "jlinkd";
  const candidates = [path.join(extensionPath, "bin", executable), path.join(os.homedir(), ".cargo", "bin", executable)];
  return candidates.find((candidate) => fs.existsSync(candidate)) ?? configured;
}

/** `jlinkd dap` starts the background process itself when needed. */
class AdapterFactory implements vscode.DebugAdapterDescriptorFactory {
  constructor(private readonly extensionPath: string) {}

  createDebugAdapterDescriptor(): vscode.DebugAdapterDescriptor {
    return new vscode.DebugAdapterExecutable(adapterPath(this.extensionPath), ["dap"]);
  }
}

function describeSpeed(speed: unknown): string {
  if (typeof speed === "number") {
    return speed >= 1000 ? `${speed / 1000}MHz` : `${speed}kHz`;
  }
  return "4MHz";
}

interface Stats {
  hss: { records: number; lost: number };
}

class StatusBar implements vscode.Disposable {
  private readonly item = vscode.window.createStatusBarItem(vscode.StatusBarAlignment.Left, 50);
  private session: vscode.DebugSession | undefined;
  private lastRecords = 0;
  private busyText: string | undefined;
  private readonly timer: NodeJS.Timeout;

  constructor(private readonly client: DataClient) {
    this.item.command = "jlinkd.showStats";
    this.timer = setInterval(() => void this.refresh(), STATUS_MS);
  }

  connected(session: vscode.DebugSession): void {
    this.session = session;
    this.busyText = undefined;
    this.render("");
    this.item.show();
  }

  busy(text: string): void {
    this.busyText = text;
    this.item.text = `$(sync~spin) ${text}`;
    this.item.show();
  }

  idle(): void {
    this.busyText = undefined;
  }

  hide(): void {
    this.session = undefined;
    this.item.hide();
  }

  private render(detail: string): void {
    if (this.session && !this.busyText) {
      const { device, speed } = this.session.configuration;
      this.item.text = `$(plug) J-Link ${device} ● ${describeSpeed(speed)}${detail}`;
    }
  }

  private async refresh(): Promise<void> {
    if (!this.session || !this.client.connected) {
      return;
    }
    try {
      const stats = await this.client.call<Stats>("session.stats");
      const rate = stats.hss.records - this.lastRecords;
      this.lastRecords = stats.hss.records;
      this.render(rate > 0 ? ` │ HSS ${(rate / 1000).toFixed(1)}kHz ${stats.hss.lost} drop` : "");
    } catch {
      // Reconnecting; the next tick tries again.
    }
  }

  dispose(): void {
    clearInterval(this.timer);
    this.item.dispose();
  }
}

/** Run the session's preLaunchTask, resolving true if it succeeded. */
async function runBuildTask(name: string | undefined): Promise<boolean> {
  if (!name) {
    return true;
  }
  const tasks = await vscode.tasks.fetchTasks();
  const task = tasks.find((candidate) => candidate.name === name);
  if (!task) {
    void vscode.window.showErrorMessage(`jlinkd: build task "${name}" was not found`);
    return false;
  }
  const execution = await vscode.tasks.executeTask(task);
  return new Promise<boolean>((resolve) => {
    const listener = vscode.tasks.onDidEndTaskProcess((event) => {
      if (event.execution === execution) {
        listener.dispose();
        resolve(event.exitCode === 0);
      }
    });
  });
}

async function buildAndReload(status: StatusBar): Promise<void> {
  const session = vscode.debug.activeDebugSession;
  if (!session || session.type !== DEBUG_TYPE) {
    void vscode.window.showWarningMessage("jlinkd: no active debug session");
    return;
  }
  status.busy("Building");
  if (!(await runBuildTask(session.configuration.preLaunchTask))) {
    status.connected(session);
    void vscode.window.showErrorMessage("jlinkd: build failed; the target was not changed");
    return;
  }
  status.busy("Reloading");
  try {
    await session.customRequest("jlinkd/reload", { elf: session.configuration.elf });
  } catch (error) {
    void vscode.window.showErrorMessage(`jlinkd: reload failed: ${String(error)}`);
  } finally {
    status.connected(session);
  }
}

interface VariableContext {
  variable?: { evaluateName?: string; name?: string };
}

/** The expression a command applies to: a Variables entry, the editor's
 *  selection, the word under the cursor, or else whatever the user types. */
async function chooseExpression(context?: VariableContext): Promise<string | undefined> {
  const fromVariables = context?.variable?.evaluateName ?? context?.variable?.name;
  if (fromVariables) {
    return fromVariables;
  }
  const editor = vscode.window.activeTextEditor;
  if (editor) {
    const selected = editor.document.getText(editor.selection).trim();
    if (selected) {
      return selected;
    }
    const word = editor.document.getWordRangeAtPosition(editor.selection.active);
    if (word) {
      return editor.document.getText(word);
    }
  }
  return vscode.window.showInputBox({ prompt: "Expression, e.g. g_state.items[2].gain" });
}

function report(error: unknown): void {
  void vscode.window.showErrorMessage(`jlinkd: ${error instanceof Error ? error.message : String(error)}`);
}

interface AddedSeries {
  seriesId: number;
  name: string;
  actualMode: string;
  error: string | null;
}

/** Plot an expression at the HSS rate. Whatever does not fit is taken back off
 *  and reported, rather than being left to sample more slowly unnoticed. */
async function addToTimelineByHss(client: DataClient, expression: string, timeline: BackendPanel): Promise<void> {
  const { series } = await client.call<{ series: AddedSeries[] }>("timeline.add", { expr: expression, mode: "hss" });
  // "off" means set up and waiting for the Timeline's rate to be chosen.
  const failed = series.filter((item) => item.actualMode !== "hss" && item.actualMode !== "off");
  await Promise.all(failed.map((item) => client.call("timeline.remove", { seriesId: item.seriesId })));
  if (series.length > 0 && failed.length === series.length) {
    throw new Error(`${expression} cannot use HSS: ${failed[0].error ?? "no capacity left"}`);
  }
  if (failed.length > 0) {
    void vscode.window.showWarningMessage(
      `jlinkd: ${failed.length} of ${series.length} values did not fit in HSS and were not added`,
    );
  }
  await timeline.show();
}

/** While a jlinkd session runs, the built-in Add to Watch commands feed Live
 *  Watch instead. A later command registration takes precedence over the
 *  built-in one, and disposing it restores the original for other debuggers. */
class WatchTakeover implements vscode.Disposable {
  private active: vscode.Disposable[] = [];

  constructor(private readonly add: (context?: VariableContext) => Promise<void>) {}

  start(): void {
    if (this.active.length === 0) {
      this.active = BUILT_IN_ADD_TO_WATCH.map((id) =>
        vscode.commands.registerCommand(id, (context?: VariableContext) => this.add(context)),
      );
    }
  }

  dispose(): void {
    this.active.forEach((item) => item.dispose());
    this.active = [];
  }
}

function replacesWatchView(): boolean {
  return vscode.workspace.getConfiguration("jlinkd").get<boolean>("replaceWatchView", true);
}

/** Hides the built-in Watch view. Only works while the Run and Debug view is
 *  open, so it is tried again when execution first stops. */
function hideBuiltInWatch(): void {
  if (replacesWatchView()) {
    vscode.commands.executeCommand(`${BUILT_IN_WATCH_VIEW}.removeView`).then(undefined, () => undefined);
  }
}

function themeKindName(kind: vscode.ColorThemeKind): string {
  switch (kind) {
    case vscode.ColorThemeKind.Light:
      return "light";
    case vscode.ColorThemeKind.HighContrast:
      return "hc-dark";
    case vscode.ColorThemeKind.HighContrastLight:
      return "hc-light";
    default:
      return "dark";
  }
}

/** A page served by the backend, shown inside the editor. */
class BackendPanel {
  private panel: vscode.WebviewPanel | undefined;
  private themeSub: vscode.Disposable | undefined;

  constructor(
    private readonly title: string,
    private readonly url: string,
  ) {}

  async show(): Promise<void> {
    if (this.panel) {
      this.panel.reveal();
      return;
    }
    const external = await vscode.env.asExternalUri(vscode.Uri.parse(this.url));
    const origin = `${external.scheme}://${external.authority}`;
    const initialKind = themeKindName(vscode.window.activeColorTheme.kind);
    const baseHref = external.toString(true);
    const iframeSrc = `${baseHref}${baseHref.includes("?") ? "&" : "?"}theme=${initialKind}`;
    this.panel = vscode.window.createWebviewPanel(`jlinkd.${this.title}`, this.title, vscode.ViewColumn.Beside, {
      enableScripts: true,
      retainContextWhenHidden: true,
    });
    this.panel.webview.html = `<!doctype html>
<html>
  <head>
    <meta http-equiv="Content-Security-Policy" content="default-src 'none'; frame-src ${origin}; style-src 'unsafe-inline'; script-src 'unsafe-inline';" />
    <style>html, body, iframe { width: 100%; height: 100%; margin: 0; padding: 0; border: 0; overflow: hidden; background: var(--vscode-editor-background); }</style>
  </head>
  <body>
    <iframe id="frame" src="${iframeSrc}"></iframe>
    <script>
      (() => {
        const frame = document.getElementById("frame");
        const VARS = [
          "--vscode-editor-background",
          "--vscode-editor-foreground",
          "--vscode-sideBar-background",
          "--vscode-sideBar-foreground",
          "--vscode-sideBarSectionHeader-background",
          "--vscode-panel-border",
          "--vscode-widget-border",
          "--vscode-input-background",
          "--vscode-input-foreground",
          "--vscode-input-border",
          "--vscode-button-background",
          "--vscode-button-foreground",
          "--vscode-button-hoverBackground",
          "--vscode-focusBorder",
          "--vscode-descriptionForeground",
          "--vscode-errorForeground",
          "--vscode-editorWarning-foreground",
          "--vscode-list-hoverBackground",
          "--vscode-badge-background",
          "--vscode-badge-foreground",
          "--vscode-font-family",
          "--vscode-editor-font-family"
        ];
        function detectKind(overrideKind) {
          if (overrideKind) return overrideKind;
          const cl = document.body.classList;
          if (cl.contains("vscode-high-contrast-light")) return "hc-light";
          if (cl.contains("vscode-high-contrast")) return "hc-dark";
          if (cl.contains("vscode-light")) return "light";
          return "dark";
        }
        function sendTheme(overrideKind) {
          if (!frame || !frame.contentWindow) return;
          const style = getComputedStyle(document.documentElement);
          const vars = {};
          for (const name of VARS) {
            const val = style.getPropertyValue(name).trim();
            if (val) vars[name] = val;
          }
          frame.contentWindow.postMessage({
            type: "jlinkd:vscode-theme",
            kind: detectKind(overrideKind),
            themeName: document.body.dataset.vscodeThemeName || "",
            vars
          }, "*");
        }
        frame.addEventListener("load", () => sendTheme());
        window.addEventListener("message", (event) => {
          if (!event.data || typeof event.data !== "object") return;
          if (event.data.type === "jlinkd:request-theme") {
            sendTheme();
          } else if (event.data.type === "jlinkd:host-theme") {
            sendTheme(event.data.kind);
          }
        });
        const observer = new MutationObserver(() => sendTheme());
        observer.observe(document.documentElement, { attributes: true, attributeFilter: ["style", "class"] });
        observer.observe(document.body, {
          attributes: true,
          attributeFilter: ["class", "data-vscode-theme-kind", "data-vscode-theme-name"]
        });
      })();
    </script>
  </body>
</html>`;
    this.themeSub = vscode.window.onDidChangeActiveColorTheme((theme) => {
      void this.panel?.webview.postMessage({
        type: "jlinkd:host-theme",
        kind: themeKindName(theme.kind),
      });
    });
    this.panel.onDidDispose(() => {
      this.themeSub?.dispose();
      this.themeSub = undefined;
      this.panel = undefined;
    });
  }
}

/** RTT channel 0 as an interactive terminal. */
class RttTerminal {
  private terminal: vscode.Terminal | undefined;
  private readonly output = new vscode.EventEmitter<string>();
  private open = false;

  constructor(private readonly client: DataClient) {
    client.on("rtt", (channel: number, data: Buffer) => {
      if (channel === RTT_TERMINAL_CHANNEL && this.open) {
        this.output.fire(data.toString("utf8").replace(/\r?\n/g, "\r\n"));
      }
    });
    client.on("open", () => {
      if (this.open) {
        this.subscribe();
      }
    });
  }

  private subscribe(): void {
    this.client.call("rtt.subscribe", { ch: RTT_TERMINAL_CHANNEL }).catch(report);
  }

  show(): void {
    if (this.terminal && this.terminal.exitStatus === undefined) {
      this.terminal.show();
      return;
    }
    const pty: vscode.Pseudoterminal = {
      onDidWrite: this.output.event,
      open: () => {
        this.open = true;
        if (this.client.connected) {
          this.subscribe();
        }
      },
      close: () => {
        this.open = false;
        if (this.client.connected) {
          this.client.call("rtt.unsubscribe", { ch: RTT_TERMINAL_CHANNEL }).catch(() => undefined);
        }
      },
      handleInput: (data) => {
        const text = data.replace(/\r/g, "\n");
        this.client.call("rtt.write", { ch: RTT_TERMINAL_CHANNEL, data: text }).catch(report);
      },
    };
    this.terminal = vscode.window.createTerminal({ name: "RTT", pty });
    this.terminal.show();
  }
}

/** Fills in the project file location, which also tells the backend the workspace. */
class ConfigurationProvider implements vscode.DebugConfigurationProvider {
  resolveDebugConfiguration(
    folder: vscode.WorkspaceFolder | undefined,
    configuration: vscode.DebugConfiguration,
  ): vscode.DebugConfiguration {
    if (!configuration.project && folder) {
      configuration.project = path.join(folder.uri.fsPath, ".jlinkd", "project.yaml");
    }
    return configuration;
  }
}

export function activate(context: vscode.ExtensionContext): void {
  registerDeviceAssist(context, () => adapterPath(context.extensionPath));
  const client = new DataClient();
  const status = new StatusBar(client);
  const liveWatch = new LiveWatch(client);
  const timeline = new BackendPanel("Timeline", BACKEND_URL);
  const rttPanel = new BackendPanel("RTT", RTT_URL);
  const rttTerminal = new RttTerminal(client);
  const timelineButton = vscode.window.createStatusBarItem(vscode.StatusBarAlignment.Left, 49);
  timelineButton.text = "$(graph-line) Timeline";
  timelineButton.tooltip = "Open the jlinkd Timeline";
  timelineButton.command = "jlinkd.openTimeline";
  const rttButton = vscode.window.createStatusBarItem(vscode.StatusBarAlignment.Left, 48);
  rttButton.text = "$(terminal) RTT";
  rttButton.tooltip = "Open the RTT terminal";
  rttButton.command = "jlinkd.openRttTerminal";
  const tree = vscode.window.createTreeView("jlinkd.liveWatch", {
    treeDataProvider: liveWatch,
    canSelectMany: true,
  });
  const peripherals = new PeripheralsView();
  const tasks = new TasksView();
  const cyclesItem = vscode.window.createStatusBarItem(vscode.StatusBarAlignment.Left, 47);
  cyclesItem.tooltip = "Cycles the core ran between the last two stops";
  let recording = false;
  let hidePending = false;
  const addFromDebugger = async (item?: VariableContext) => {
    const expression = await chooseExpression(item);
    if (expression) {
      client.start();
      const added = await liveWatch.add(expression);
      await tree.reveal(added, { select: true, focus: false });
    }
  };
  const takeover = new WatchTakeover((item) => addFromDebugger(item).catch(report));
  const output = vscode.window.createOutputChannel("jlinkd");

  const syncWatches = () => {
    if (client.connected) {
      liveWatch.sync().catch(report);
    }
  };
  client.on("open", syncWatches);

  context.subscriptions.push(
    status,
    tree,
    timelineButton,
    rttButton,
    takeover,
    cyclesItem,
    vscode.window.registerTreeDataProvider("jlinkd.peripherals", peripherals),
    vscode.window.registerTreeDataProvider("jlinkd.tasks", tasks),
    vscode.debug.onDidChangeActiveStackItem(() => {
      if (hidePending && vscode.debug.activeStackItem) {
        hidePending = false;
        hideBuiltInWatch();
      }
      if (vscode.debug.activeStackItem && vscode.debug.activeDebugSession?.type === DEBUG_TYPE) {
        peripherals.refresh();
        tasks.refresh();
      }
    }),
    vscode.debug.registerDebugConfigurationProvider(DEBUG_TYPE, new ConfigurationProvider()),
    output,
    tree.onDidCollapseElement((event) => liveWatch.collapse(event.element).catch(report)),
    vscode.debug.registerDebugAdapterDescriptorFactory(DEBUG_TYPE, new AdapterFactory(context.extensionPath)),
    vscode.debug.onDidStartDebugSession((session) => {
      if (session.type === DEBUG_TYPE) {
        status.connected(session);
        timelineButton.show();
        rttButton.show();
        client.start();
        takeover.start();
        hidePending = true;
        hideBuiltInWatch();
      }
    }),
    vscode.debug.onDidTerminateDebugSession((session) => {
      if (session.type === DEBUG_TYPE) {
        status.hide();
        timelineButton.hide();
        rttButton.hide();
        takeover.dispose();
        cyclesItem.hide();
        peripherals.clear();
        tasks.refresh();
        // A connection left open would keep the idle backend from exiting.
        client.stop();
      }
    }),
    vscode.debug.onDidReceiveDebugSessionCustomEvent((event) => {
      if (event.event === "jlinkd/bound" || event.event === "jlinkd/reloaded") {
        syncWatches();
      } else if (event.event === "jlinkd/fault") {
        void showFault(event.body, output);
      } else if (event.event === "jlinkd/cycles") {
        cyclesItem.text = describeCycles(event.body);
        cyclesItem.show();
      }
    }),
    vscode.commands.registerCommand("jlinkd.buildAndReload", () => buildAndReload(status)),
    vscode.commands.registerCommand("jlinkd.refreshPeripherals", () => peripherals.refresh()),
    vscode.commands.registerCommand("jlinkd.refreshTasks", () => tasks.refresh()),
    vscode.commands.registerCommand("jlinkd.readRegister", (node: PeripheralNode) => {
      if (node.kind === "register") {
        peripherals.readRegister(node).catch(report);
      }
    }),
    vscode.commands.registerCommand("jlinkd.writePeripheral", (node: PeripheralNode) => {
      if (node.kind !== "peripheral") {
        peripherals.write(node).catch(report);
      }
    }),
    vscode.commands.registerCommand("jlinkd.addToWatch", async (item?: VariableContext) => {
      const expression = await chooseExpression(item);
      if (expression) {
        await liveWatch.add(expression).catch(report);
      }
    }),
    vscode.commands.registerCommand("jlinkd.addSelectionToWatch", () =>
      addFromDebugger().catch(report),
    ),
    vscode.commands.registerCommand("jlinkd.addToTimeline", async (item?: VariableContext | WatchInfo) => {
      if (item && "expr" in item && !item.plottable) {
        report(new Error(`${item.name} cannot be plotted by itself`));
        return;
      }
      const fromWatch = item && "expr" in item ? item.expr : undefined;
      const expression = fromWatch ?? (await chooseExpression(item as VariableContext | undefined));
      if (expression) {
        await addToTimelineByHss(client, expression, timeline).catch(report);
      }
    }),
    vscode.commands.registerCommand("jlinkd.addSelectionToTimeline", async (source?: string) => {
      if (source === "liveWatch" || (source !== "editor" && !vscode.window.activeTextEditor && tree.selection.length > 0)) {
        const selected = tree.selection;
        if (selected.length === 0) {
          return;
        }
        const plottable = selected.filter((item) => item.plottable);
        const skipped = selected.filter((item) => !item.plottable);
        if (plottable.length === 0) {
          report(new Error(`${skipped[0].name} cannot be plotted by itself`));
          return;
        }
        if (skipped.length > 0) {
          void vscode.window.showWarningMessage(
            `jlinkd: ${skipped.length} selected item(s) cannot be plotted directly`,
          );
        }
        for (const item of plottable) {
          await addToTimelineByHss(client, item.expr, timeline).catch(report);
        }
        return;
      }
      const expression = await chooseExpression();
      if (expression) {
        await addToTimelineByHss(client, expression, timeline).catch(report);
      }
    }),
    vscode.commands.registerCommand("jlinkd.removeSelectedWatch", async () => {
      const selected = tree.selection;
      if (selected.length === 0) {
        return;
      }
      const ids = new Set(selected.map((item) => item.id));
      const top = selected.filter((item) => item.parent === null || !ids.has(item.parent));
      for (const item of top) {
        await liveWatch.remove(item).catch(report);
      }
    }),
    vscode.commands.registerCommand("jlinkd.editSelectedWatch", async (trigger?: string) => {
      if (tree.selection.length !== 1) {
        return;
      }
      const item = tree.selection[0];
      if (!item.writable) {
        if (trigger === "enter" && item.expandable) {
          await vscode.commands.executeCommand("list.toggleExpand");
        }
        return;
      }
      const value = await vscode.window.showInputBox({
        prompt: `New value for ${item.name}`,
        value: item.value,
      });
      if (value !== undefined) {
        await liveWatch.write(item, value).catch(report);
      }
    }),
    vscode.commands.registerCommand("jlinkd.setSelectedWatchRate", async () => {
      const selected = tree.selection;
      if (selected.length === 0) {
        return;
      }
      const label = selected.length === 1 ? selected[0].name : `${selected.length} items`;
      const choice = await vscode.window.showQuickPick(RATES, {
        placeHolder: `Refresh rate for ${label}`,
      });
      if (!choice) {
        return;
      }
      for (const item of selected) {
        await liveWatch.setRate(item, choice.rateHz).catch(report);
      }
    }),
    vscode.commands.registerCommand("jlinkd.copySelectedWatch", async () => {
      const selected = tree.selection;
      if (selected.length === 0) {
        return;
      }
      const text = selected.map((item) => `${item.expr} = ${item.value}`).join("\n");
      await vscode.env.clipboard.writeText(text);
    }),
    vscode.commands.registerCommand("jlinkd.removeWatch", (item: WatchInfo) =>
      liveWatch.remove(item).catch(report),
    ),
    vscode.commands.registerCommand("jlinkd.setWatchRate", async (item: WatchInfo) => {
      const choice = await vscode.window.showQuickPick(RATES, { placeHolder: `Refresh rate for ${item.name}` });
      if (choice) {
        await liveWatch.setRate(item, choice.rateHz).catch(report);
      }
    }),
    vscode.commands.registerCommand("jlinkd.writeWatch", async (item: WatchInfo) => {
      const value = await vscode.window.showInputBox({ prompt: `New value for ${item.name}`, value: item.value });
      if (value !== undefined) {
        await liveWatch.write(item, value).catch(report);
      }
    }),
    vscode.commands.registerCommand("jlinkd.openTimeline", () => timeline.show().catch(report)),
    vscode.commands.registerCommand("jlinkd.openRttTerminal", () => rttTerminal.show()),
    vscode.commands.registerCommand("jlinkd.openRttPanel", () => rttPanel.show().catch(report)),
    vscode.commands.registerCommand("jlinkd.openTimelineInBrowser", () =>
      vscode.env.openExternal(vscode.Uri.parse(BACKEND_URL)),
    ),
    vscode.commands.registerCommand("jlinkd.startRecording", async () => {
      try {
        if (recording) {
          const result = await client.call<{ path: string; samples: number }>("record.stop");
          void vscode.window.showInformationMessage(`jlinkd: saved ${result.samples} samples to ${result.path}`);
        } else {
          await client.call("record.start");
          void vscode.window.showInformationMessage("jlinkd: recording; run this command again to stop");
        }
        recording = !recording;
      } catch (error) {
        report(error);
      }
    }),
    vscode.commands.registerCommand("jlinkd.showStats", async () => {
      try {
        output.clear();
        output.appendLine(JSON.stringify(await client.call("session.stats"), null, 2));
        output.show(true);
      } catch (error) {
        report(error);
      }
    }),
  );
}

export function deactivate(): void {}
