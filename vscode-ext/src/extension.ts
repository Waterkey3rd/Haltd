import * as fs from "fs";
import * as os from "os";
import * as path from "path";
import * as vscode from "vscode";
import { BACKEND_PORT, DataClient } from "./dataClient";
import {
  describeCycles,
  PeripheralsView,
  showFault,
  TasksView,
  type PeripheralNode,
} from "./debugViews";
import { registerDeviceAssist } from "./deviceAssist";
import { DiagnosticsManager } from "./errors";
import { LiveWatchController } from "./liveWatch";

const DEBUG_TYPE = "haltd";
const STATUS_MS = 1000;
const RTT_TERMINAL_CHANNEL = 0;
const BUILT_IN_WATCH_VIEW = "workbench.debug.watchExpressionsView";
const BUILT_IN_ADD_TO_WATCH = ["debug.addToWatchExpressions", "editor.debug.action.selectionToWatch"];
const BACKEND_URL = `http://127.0.0.1:${BACKEND_PORT}/timeline`;
const RTT_URL = `http://127.0.0.1:${BACKEND_PORT}/rtt`;

/** The configured path, then the copy packaged with the extension, then
 *  ~/.cargo/bin, which VS Code started from a desktop launcher often lacks on PATH. */
export function adapterPath(extensionPath: string): string {
  const configured = vscode.workspace.getConfiguration("haltd").get<string>("path", "haltd");
  if (configured !== "haltd") {
    return configured;
  }
  const executable = process.platform === "win32" ? "haltd.exe" : "haltd";
  const candidates = [path.join(extensionPath, "bin", executable), path.join(os.homedir(), ".cargo", "bin", executable)];
  return candidates.find((candidate) => fs.existsSync(candidate)) ?? configured;
}

/** `haltd dap` starts the background process itself when needed. */
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
    this.item.command = "haltd.showStats";
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
async function runBuildTask(
  name: string | undefined,
  diagnostics: DiagnosticsManager,
): Promise<boolean> {
  if (!name) {
    return true;
  }
  const tasks = await vscode.tasks.fetchTasks();
  const task = tasks.find((candidate) => candidate.name === name);
  if (!task) {
    void diagnostics.reportError(
      new Error(`build task "${name}" was not found`),
      "Build & Reload",
    );
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

async function buildAndReload(
  status: StatusBar,
  diagnostics: DiagnosticsManager,
): Promise<void> {
  const session = vscode.debug.activeDebugSession;
  if (!session || session.type !== DEBUG_TYPE) {
    void vscode.window.showWarningMessage("Haltd: no active debug session");
    return;
  }
  status.busy("Building");
  if (!(await runBuildTask(session.configuration.preLaunchTask, diagnostics))) {
    status.connected(session);
    void diagnostics.reportError(
      new Error("build failed; the target was not changed"),
      "Build & Reload",
    );
    return;
  }
  status.busy("Reloading");
  try {
    await session.customRequest("haltd/reload", { elf: session.configuration.elf });
  } catch (error) {
    void diagnostics.reportError(error, "Build & Reload");
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
      `Haltd: ${failed.length} of ${series.length} values did not fit in HSS and were not added`,
    );
  }
  await timeline.show();
}

/** While a Haltd session runs, the built-in Add to Watch commands feed Live
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
  return vscode.workspace.getConfiguration("haltd").get<boolean>("replaceWatchView", true);
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

interface SaveExportRequest {
  type: "haltd:save-export";
  requestId: number;
  kind: "csv" | "hlrec" | "png";
  defaultFilename: string;
  params?: Record<string, unknown>;
  dataUrl?: string;
}

/** A page served by the backend, shown inside the editor with native save-dialog bridge. */
class BackendPanel {
  private panel: vscode.WebviewPanel | undefined;
  private themeSub: vscode.Disposable | undefined;
  private lastSaveDir: string | undefined;

  constructor(
    private readonly title: string,
    private readonly url: string,
    private readonly client?: DataClient,
    private readonly onError?: (error: unknown, context: string) => void,
  ) {}

  private defaultSaveUri(filename: string): vscode.Uri {
    const baseDir = this.lastSaveDir ?? vscode.workspace.workspaceFolders?.[0]?.uri.fsPath ?? os.homedir();
    return vscode.Uri.file(path.join(baseDir, filename));
  }

  private async handleSaveExport(req: SaveExportRequest): Promise<void> {
    const reply = (ok: boolean, message?: string, savedPath?: string) => {
      void this.panel?.webview.postMessage({
        type: "haltd:save-export-result",
        requestId: req.requestId,
        ok,
        message,
        path: savedPath,
      });
    };

    const filters: Record<string, string[]> =
      req.kind === "csv"
        ? { "CSV Files": ["csv"] }
        : req.kind === "hlrec"
          ? { "Haltd Recording": ["hlrec"] }
          : { "PNG Image": ["png"] };

    try {
      const targetUri = await vscode.window.showSaveDialog({
        defaultUri: this.defaultSaveUri(req.defaultFilename),
        filters,
        saveLabel: `Save ${req.kind.toUpperCase()}`,
      });
      if (!targetUri) {
        reply(false, "cancelled");
        return;
      }
      this.lastSaveDir = path.dirname(targetUri.fsPath);

      let summary = `Saved ${path.basename(targetUri.fsPath)}`;
      if (req.kind === "png") {
        const encoded = req.dataUrl?.split(",")[1];
        if (!encoded) {
          throw new Error("missing PNG image data");
        }
        fs.writeFileSync(targetUri.fsPath, Buffer.from(encoded, "base64"));
      } else if (req.kind === "hlrec") {
        if (!this.client) {
          throw new Error("data client unavailable");
        }
        await this.client.call("record.saveHlrec", {
          ...(req.params ?? {}),
          path: targetUri.fsPath,
        });
      } else {
        if (!this.client) {
          throw new Error("data client unavailable");
        }
        const res = await this.client.call<{ path: string; rows: number }>("record.export", {
          format: "csv",
          ...(req.params ?? {}),
          path: targetUri.fsPath,
        });
        summary = `Exported ${res.rows} rows to ${path.basename(targetUri.fsPath)}`;
      }

      reply(true, summary, targetUri.fsPath);
      const actionButtons = req.kind === "csv" ? ["Open File", "Reveal in Folder"] : ["Reveal in Folder"];
      const choice = await vscode.window.showInformationMessage(`Haltd: ${summary}`, ...actionButtons);
      if (choice === "Open File") {
        const doc = await vscode.workspace.openTextDocument(targetUri);
        await vscode.window.showTextDocument(doc, { preview: false });
      } else if (choice === "Reveal in Folder") {
        await vscode.commands.executeCommand("revealFileInOS", targetUri);
      }
    } catch (error) {
      const msg = error instanceof Error ? error.message : String(error);
      reply(false, msg);
      this.onError?.(error, `Export ${req.kind.toUpperCase()}`);
    }
  }

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
    this.panel = vscode.window.createWebviewPanel(`haltd.${this.title}`, this.title, vscode.ViewColumn.Beside, {
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
        const vscodeApi = acquireVsCodeApi();
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
            type: "haltd:vscode-theme",
            kind: detectKind(overrideKind),
            themeName: document.body.dataset.vscodeThemeName || "",
            vars
          }, "*");
        }
        frame.addEventListener("load", () => sendTheme());
        window.addEventListener("message", (event) => {
          if (!event.data || typeof event.data !== "object") return;
          if (event.data.type === "haltd:request-theme") {
            sendTheme();
          } else if (event.data.type === "haltd:host-theme") {
            sendTheme(event.data.kind);
          } else if (event.data.type === "haltd:save-export") {
            vscodeApi.postMessage(event.data);
          } else if (event.data.type === "haltd:save-export-result") {
            frame?.contentWindow?.postMessage(event.data, "*");
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
    this.panel.webview.onDidReceiveMessage((msg: unknown) => {
      if (msg && typeof msg === "object" && (msg as SaveExportRequest).type === "haltd:save-export") {
        void this.handleSaveExport(msg as SaveExportRequest);
      }
    });
    this.themeSub = vscode.window.onDidChangeActiveColorTheme((theme) => {
      void this.panel?.webview.postMessage({
        type: "haltd:host-theme",
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

  constructor(
    private readonly client: DataClient,
    private readonly onError: (error: unknown, context: string) => void,
  ) {
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
    this.client
      .call("rtt.subscribe", { ch: RTT_TERMINAL_CHANNEL })
      .catch((e) => this.onError(e, "RTT Subscribe"));
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
        this.client
          .call("rtt.write", { ch: RTT_TERMINAL_CHANNEL, data: text })
          .catch((e) => this.onError(e, "RTT Write"));
      },
    };
    this.terminal = vscode.window.createTerminal({ name: "RTT", pty });
    this.terminal.show();
  }
}

/** Fills in the project file location and checks launch prerequisites. */
class ConfigurationProvider implements vscode.DebugConfigurationProvider {
  constructor(private readonly diagnostics: DiagnosticsManager) {}

  resolveDebugConfiguration(
    folder: vscode.WorkspaceFolder | undefined,
    configuration: vscode.DebugConfiguration,
  ): vscode.DebugConfiguration {
    if (!configuration.project && folder) {
      configuration.project = path.join(folder.uri.fsPath, ".haltd", "project.yaml");
    }
    if (configuration.rtosThreadView === undefined) {
      configuration.rtosThreadView = vscode.workspace
        .getConfiguration("haltd", folder?.uri)
        .get<string>("rtos.threadView", "currentOnly");
    }
    return configuration;
  }

  resolveDebugConfigurationWithSubstitutedVariables(
    _folder: vscode.WorkspaceFolder | undefined,
    configuration: vscode.DebugConfiguration,
  ): vscode.DebugConfiguration | undefined {
    if (!this.diagnostics.preflightLaunch(configuration)) {
      return undefined;
    }
    return configuration;
  }
}

interface PresetSummary {
  name: string;
  rateHz?: number | null;
  signals: Array<{
    kind: string;
    expr: string;
    mode: string;
    group?: string | null;
    color?: string | null;
  }>;
}

export function activate(context: vscode.ExtensionContext): void {
  const client = new DataClient();
  const diagnostics = new DiagnosticsManager(context, () => adapterPath(context.extensionPath), client);
  const report = (error: unknown, source = "Command"): void => {
    void diagnostics.reportError(error, source);
  };

  registerDeviceAssist(context, () => adapterPath(context.extensionPath), report);
  const status = new StatusBar(client);
  const timeline = new BackendPanel("Timeline", BACKEND_URL, client, report);
  const rttPanel = new BackendPanel("RTT", RTT_URL, client, report);
  const liveWatch = new LiveWatchController(context.extensionUri, client, {
    onAddToTimeline: (expr) => addToTimelineByHss(client, expr, timeline),
    onError: report,
  });
  const rttTerminal = new RttTerminal(client, report);
  const timelineButton = vscode.window.createStatusBarItem(vscode.StatusBarAlignment.Left, 49);
  timelineButton.text = "$(graph-line) Timeline";
  timelineButton.tooltip = "Open the Haltd Timeline";
  timelineButton.command = "haltd.openTimeline";
  const rttButton = vscode.window.createStatusBarItem(vscode.StatusBarAlignment.Left, 48);
  rttButton.text = "$(terminal) RTT";
  rttButton.tooltip = "Open the RTT terminal";
  rttButton.command = "haltd.openRttTerminal";
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
      await liveWatch.add(expression, true);
    }
  };
  const takeover = new WatchTakeover((item) => addFromDebugger(item).catch((e) => report(e, "Add to Watch")));

  const syncWatches = () => {
    if (client.connected) {
      liveWatch.sync().catch((e) => report(e, "Live Watch Sync"));
    }
  };
  client.on("open", () => {
    diagnostics.logInfo("WebSocket 数据通道已连接");
    syncWatches();
  });
  client.on("close", (info) => {
    diagnostics.recordWsClose(info);
    liveWatch.setRunState("disconnected");
  });

  const runStateTracker: vscode.DebugAdapterTrackerFactory = {
    createDebugAdapterTracker: () => ({
      onWillReceiveMessage: (message: unknown) => {
        const msg = message as { type?: string; command?: string };
        if (msg?.type === "request" && (msg.command === "continue" || msg.command === "next" || msg.command === "stepIn" || msg.command === "stepOut")) {
          liveWatch.setRunState("running");
        }
      },
      onDidSendMessage: (message: unknown) => {
        const msg = message as { type?: string; event?: string };
        if (msg?.type === "event") {
          if (msg.event === "stopped") {
            liveWatch.setRunState("halted");
          } else if (msg.event === "continued") {
            liveWatch.setRunState("running");
          }
        }
      },
    }),
  };

  context.subscriptions.push(
    diagnostics,
    status,
    liveWatch,
    timelineButton,
    rttButton,
    takeover,
    cyclesItem,
    vscode.window.registerWebviewViewProvider("haltd.liveWatch", liveWatch, {
      webviewOptions: { retainContextWhenHidden: true },
    }),
    vscode.window.registerTreeDataProvider("haltd.peripherals", peripherals),
    vscode.window.registerTreeDataProvider("haltd.tasks", tasks),
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
    vscode.debug.registerDebugConfigurationProvider(DEBUG_TYPE, new ConfigurationProvider(diagnostics)),
    vscode.debug.registerDebugAdapterTrackerFactory(DEBUG_TYPE, diagnostics.createTrackerFactory()),
    vscode.debug.registerDebugAdapterTrackerFactory(DEBUG_TYPE, runStateTracker),
    vscode.debug.registerDebugAdapterDescriptorFactory(DEBUG_TYPE, new AdapterFactory(context.extensionPath)),
    vscode.debug.onDidStartDebugSession((session) => {
      if (session.type === DEBUG_TYPE) {
        status.connected(session);
        timelineButton.show();
        rttButton.show();
        client.start();
        takeover.start();
        liveWatch.setRunState("running");
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
        liveWatch.setRunState("disconnected");
        // A connection left open would keep the idle backend from exiting.
        client.stop();
      }
    }),
    vscode.debug.onDidReceiveDebugSessionCustomEvent((event) => {
      if (event.event === "haltd/bound" || event.event === "haltd/reloaded") {
        syncWatches();
      } else if (event.event === "haltd/fault") {
        void showFault(event.body, diagnostics);
      } else if (event.event === "haltd/cycles") {
        cyclesItem.text = describeCycles(event.body);
        cyclesItem.show();
      }
    }),
    vscode.commands.registerCommand("haltd.buildAndReload", () => buildAndReload(status, diagnostics)),
    vscode.commands.registerCommand("haltd.refreshPeripherals", () => peripherals.refresh()),
    vscode.commands.registerCommand("haltd.refreshTasks", () => tasks.refresh()),
    vscode.commands.registerCommand("haltd.readRegister", (node: PeripheralNode) => {
      if (node.kind === "register") {
        peripherals.readRegister(node).catch((e) => report(e, "Read Peripheral Register"));
      }
    }),
    vscode.commands.registerCommand("haltd.writePeripheral", (node: PeripheralNode) => {
      if (node.kind !== "peripheral") {
        peripherals.write(node).catch((e) => report(e, "Write Peripheral"));
      }
    }),
    vscode.commands.registerCommand("haltd.addToWatch", async (item?: VariableContext) => {
      const fromVariables = item?.variable?.evaluateName ?? item?.variable?.name;
      if (fromVariables) {
        client.start();
        await liveWatch.add(fromVariables, true).catch((e) => report(e, "Add to Watch"));
        return;
      }
      const editor = vscode.window.activeTextEditor;
      if (editor && editor.document === vscode.window.activeTextEditor?.document && vscode.debug.activeDebugSession?.type === DEBUG_TYPE) {
        const selected = editor.document.getText(editor.selection).trim();
        const word = !selected ? editor.document.getWordRangeAtPosition(editor.selection.active) : undefined;
        const expr = selected || (word ? editor.document.getText(word) : undefined);
        if (expr && item) {
          client.start();
          await liveWatch.add(expr, true).catch((e) => report(e, "Add to Watch"));
          return;
        }
      }
      await liveWatch.focusInputOrReveal().catch((e) => report(e, "Add to Watch"));
    }),
    vscode.commands.registerCommand("haltd.addSelectionToWatch", () =>
      addFromDebugger().catch((e) => report(e, "Add Selection to Watch")),
    ),
    vscode.commands.registerCommand("haltd.popOutLiveWatch", () =>
      liveWatch.popOut().catch((e) => report(e, "Pop Out Live Watch")),
    ),
    vscode.commands.registerCommand("haltd.dockLiveWatchToPanel", () =>
      liveWatch.dockToPanel().catch((e) => report(e, "Dock Live Watch")),
    ),
    vscode.commands.registerCommand("haltd.configureWatchColumns", () =>
      liveWatch.configureColumnsPicker().catch((e) => report(e, "Configure Live Watch Columns")),
    ),
    vscode.commands.registerCommand("haltd.addToTimeline", async (item?: VariableContext) => {
      const expression = await chooseExpression(item);
      if (expression) {
        await addToTimelineByHss(client, expression, timeline).catch((e) => report(e, "Add to Timeline"));
      }
    }),
    vscode.commands.registerCommand("haltd.addSelectionToTimeline", async () => {
      const expression = await chooseExpression();
      if (expression) {
        await addToTimelineByHss(client, expression, timeline).catch((e) => report(e, "Add Selection to Timeline"));
      }
    }),
    vscode.commands.registerCommand("haltd.loadTimelinePreset", async () => {
      try {
        const reply = await client.call<{ presets: PresetSummary[] }>("timeline.presets.list");
        const presets = reply.presets ?? [];
        if (presets.length === 0) {
          void vscode.window.showInformationMessage("Haltd: no saved Timeline presets in this workspace yet");
          return;
        }
        const picked = await vscode.window.showQuickPick(
          presets.map((p) => ({
            label: p.name,
            description: `${p.signals.length} signals${p.rateHz ? ` · ${p.rateHz} Hz` : ""}`,
            preset: p,
          })),
          { placeHolder: "Select Timeline preset to load" },
        );
        if (!picked) {
          return;
        }
        await client.call("timeline.applyBatch", {
          replace: true,
          ...(picked.preset.rateHz !== undefined && picked.preset.rateHz !== null
            ? { rateHz: picked.preset.rateHz }
            : {}),
          signals: picked.preset.signals,
        });
        await client.call("timeline.presets.setActive", { name: picked.preset.name });
        await timeline.show();
      } catch (error) {
        report(error, "Load Timeline Preset");
      }
    }),
    vscode.commands.registerCommand("haltd.openTimeline", () => timeline.show().catch((e) => report(e, "Open Timeline"))),
    vscode.commands.registerCommand("haltd.openRttTerminal", () => rttTerminal.show()),
    vscode.commands.registerCommand("haltd.openRttPanel", () => rttPanel.show().catch((e) => report(e, "Open RTT Panel"))),
    vscode.commands.registerCommand("haltd.openTimelineInBrowser", () =>
      vscode.env.openExternal(vscode.Uri.parse(BACKEND_URL)),
    ),
    vscode.commands.registerCommand("haltd.startRecording", async () => {
      try {
        if (recording) {
          const result = await client.call<{ path: string; samples: number }>("record.stop");
          recording = false;
          const choice = await vscode.window.showInformationMessage(
            `Haltd: recorded ${result.samples} samples`,
            "Export CSV...",
            "Reveal in Folder",
          );
          if (choice === "Export CSV...") {
            const folder = vscode.workspace.workspaceFolders?.[0]?.uri.fsPath ?? os.homedir();
            const dest = await vscode.window.showSaveDialog({
              defaultUri: vscode.Uri.file(path.join(folder, `haltd-recording-${Date.now()}.csv`)),
              filters: { "CSV Files": ["csv"] },
            });
            if (dest) {
              const exported = await client.call<{ path: string; rows: number }>("record.export", {
                format: "csv",
                fromRecording: true,
                hlrecPath: result.path,
                zoh: true,
                relativeTime: true,
                path: dest.fsPath,
              });
              const openChoice = await vscode.window.showInformationMessage(
                `Haltd: exported ${exported.rows} rows to ${path.basename(dest.fsPath)}`,
                "Open File",
              );
              if (openChoice === "Open File") {
                const doc = await vscode.workspace.openTextDocument(dest);
                await vscode.window.showTextDocument(doc, { preview: false });
              }
            }
          } else if (choice === "Reveal in Folder") {
            await vscode.commands.executeCommand("revealFileInOS", vscode.Uri.file(result.path));
          }
        } else {
          await client.call("record.start");
          recording = true;
          void vscode.window.showInformationMessage("Haltd: recording; run this command again to stop");
        }
      } catch (error) {
        report(error, "Recording");
      }
    }),
    vscode.commands.registerCommand("haltd.showStats", async () => {
      try {
        const stats = await client.call("session.stats");
        diagnostics.logInfo(`Session stats:\n${JSON.stringify(stats, null, 2)}`);
        diagnostics.outputChannel.show(true);
      } catch (error) {
        report(error, "Show Statistics");
      }
    }),
    vscode.commands.registerCommand("haltd.collectDiagnostics", () =>
      diagnostics.openDiagnosticsReport().catch((e) => report(e, "Collect Diagnostics")),
    ),
  );
}

export function deactivate(): void {}
