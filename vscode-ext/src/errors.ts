import { execFileSync } from "child_process";
import * as fs from "fs";
import * as os from "os";
import * as path from "path";
import * as vscode from "vscode";
import { BackendRpcError, type DataClient, type HaltdDiagnostic, type WsCloseInfo } from "./dataClient";

const MAX_RECENT_LOGS = 200;
const DAP_ERROR_TTL_MS = 3000;
const SERVE_LOG_TAIL_LINES = 50;
const NOISY_DAP_COMMANDS = new Set([
  "threads",
  "stackTrace",
  "scopes",
  "variables",
  "evaluate",
  "haltd/peripherals",
  "haltd/peripheralRead",
  "haltd/rtosTasks",
  "haltd/cycleCounter",
]);

export class HaltdClientError extends Error {
  constructor(readonly diagnostic: HaltdDiagnostic) {
    super(diagnostic.message);
    this.name = "HaltdClientError";
  }
}

interface RecordedDiagnostic {
  at: string;
  context: string;
  diagnostic: HaltdDiagnostic;
}

interface CachedVersion {
  adapter: string;
  mtimeMs: number;
  versionOutput: string;
}

function isHaltdDiagnostic(value: unknown): value is HaltdDiagnostic {
  if (!value || typeof value !== "object") {
    return false;
  }
  const obj = value as Partial<HaltdDiagnostic>;
  return (
    typeof obj.code === "string" &&
    typeof obj.message === "string" &&
    Array.isArray(obj.causes) &&
    Array.isArray(obj.actions)
  );
}

function sanitizeValue(value: unknown, workspaceRoots: string[]): unknown {
  if (typeof value === "string") {
    let out = value;
    for (const root of workspaceRoots) {
      if (root && out.startsWith(root)) {
        out = `<workspace>${out.slice(root.length)}`;
      }
    }
    const home = os.homedir();
    if (home && out.startsWith(home)) {
      out = `<home>${out.slice(home.length)}`;
    }
    return out;
  }
  if (Array.isArray(value)) {
    return value.map((item) => sanitizeValue(item, workspaceRoots));
  }
  if (value && typeof value === "object") {
    const result: Record<string, unknown> = {};
    for (const [key, item] of Object.entries(value)) {
      if (/token|secret|password|key/i.test(key)) {
        result[key] = "<redacted>";
      } else {
        result[key] = sanitizeValue(item, workspaceRoots);
      }
    }
    return result;
  }
  return value;
}

function sanitizeConfig(config: Record<string, unknown>): Record<string, unknown> {
  const roots = (vscode.workspace.workspaceFolders ?? []).map((f) => f.uri.fsPath);
  return sanitizeValue(config, roots) as Record<string, unknown>;
}

function findServeLogPath(): string | undefined {
  const candidates: string[] = [];
  if (process.env.HALTD_CACHE_DIR) {
    candidates.push(path.join(process.env.HALTD_CACHE_DIR, "serve.log"));
  }
  if (process.platform === "win32") {
    candidates.push(path.join(os.homedir(), "Documents", "haltd", "serve.log"));
    if (process.env.LOCALAPPDATA) {
      candidates.push(path.join(process.env.LOCALAPPDATA, "haltd", "serve.log"));
    }
  } else {
    const xdg = process.env.XDG_CACHE_HOME;
    if (xdg) {
      candidates.push(path.join(xdg, "haltd", "serve.log"));
    }
    candidates.push(path.join(os.homedir(), ".cache", "haltd", "serve.log"));
  }
  return candidates.find((candidate) => fs.existsSync(candidate));
}

function readLogTail(filePath: string, maxLines: number): string | undefined {
  try {
    const raw = fs.readFileSync(filePath, "utf8");
    const lines = raw.trimEnd().split(/\r?\n/);
    return lines.slice(-maxLines).join("\n");
  } catch {
    return undefined;
  }
}

function parseSemverFromOutput(output: string): string | undefined {
  const match = output.match(/(\d+\.\d+\.\d+(?:[-+][^\s]+)?)/);
  return match?.[1];
}

function wsCloseExplanation(info: WsCloseInfo): { cause: string; action: string } {
  if (info.code === 1006) {
    const errDetail = info.error ? `（${info.error}）` : "";
    return {
      cause: `数据通道非正常断开（code 1006${errDetail}）：后台守护进程尚未启动、已崩溃退出，或 47110 端口被冲突进程占用`,
      action: "检查 ~/.cache/haltd/serve.log，确认没有旧版本或冲突的进程占用 47110 端口",
    };
  }
  if (info.code === 1000 || info.code === 1001) {
    return {
      cause: `后台守护进程主动关闭连接（code ${info.code}${info.reason ? `: ${info.reason}` : ""}）`,
      action: "若会话仍在进行中，扩展会自动尝试重连",
    };
  }
  return {
    cause: `WebSocket 连接关闭（code ${info.code}${info.reason ? `: ${info.reason}` : ""}）`,
    action: "查看 Haltd 日志通道与 serve.log 排查异常断开原因",
  };
}

function adapterExitDiagnostic(
  code: number | undefined,
  signal: string | undefined,
  adapter: string,
): HaltdDiagnostic {
  const detail = `adapter="${adapter}", exitCode=${code ?? "null"}, signal=${signal ?? "none"}`;
  if (code === 126) {
    return {
      id: 2001,
      code: "E_ADAPTER_EXEC",
      message: `调试适配器无法执行（退出码 126）`,
      detail,
      causes: [
        "haltd 可执行文件缺少执行权限（+x）",
        "文件所在挂载分区设置了 noexec 或被系统安全策略拦截",
      ],
      actions: [`对 ${adapter} 执行 chmod +x，或检查文件权限与 haltd.path 设置`],
    };
  }
  if (code === 127) {
    return {
      id: 2001,
      code: "E_ADAPTER_EXEC",
      message: "找不到调试适配器二进制或其依赖的动态库（退出码 127）",
      detail,
      causes: [
        `找不到可执行文件 "${adapter}"`,
        "或程序启动时缺少必要的系统共享库",
      ],
      actions: ["检查 haltd.path 配置、扩展安装包完整性或 ~/.cargo/bin/haltd"],
    };
  }
  if (code === 101 || signal === "SIGABRT" || signal === "SIGSEGV") {
    return {
      id: 2099,
      code: "E_ADAPTER_CRASH",
      message: `调试适配器异常崩溃（${signal ?? `退出码 ${code}`}）`,
      detail,
      causes: [
        "haltd 进程发生未捕获 panic 或在加载/调用 J-Link DLL 时发生原生崩溃",
      ],
      actions: ["运行 \"Haltd: Collect Diagnostics\" 查看 serve.log 末尾栈信息并提交反馈"],
    };
  }
  return {
    id: 2002,
    code: "E_ADAPTER_EXIT",
    message: `调试适配器非正常退出（退出码 ${code ?? "null"}${signal ? `，信号 ${signal}` : ""}）`,
    detail,
    causes: [
      "后台守护进程启动超时或被另一个不同版本的 haltd 进程替代后尚未就绪",
      "端口 47110/47111 被其他进程占用",
    ],
    actions: ["重试启动调试；若持续失败，请查看 serve.log 并运行 \"Haltd: Collect Diagnostics\""],
  };
}

export class DiagnosticsManager implements vscode.Disposable {
  readonly outputChannel: vscode.LogOutputChannel;
  private readonly recentLogs: string[] = [];
  private readonly recentDapErrors: Array<{ command: string; at: number; diagnostic: HaltdDiagnostic }> = [];
  private lastDiagnostic: RecordedDiagnostic | undefined;
  private lastLaunchConfig: Record<string, unknown> | undefined;
  private lastWsClose: (WsCloseInfo & { at: string }) | undefined;
  private versionCache: CachedVersion | undefined;

  constructor(
    private readonly context: vscode.ExtensionContext,
    private readonly getAdapterPath: () => string,
    private readonly client: DataClient,
  ) {
    this.outputChannel = vscode.window.createOutputChannel("Haltd", { log: true });
  }

  private pushLog(level: "INFO" | "WARN" | "ERROR" | "DEBUG", message: string): void {
    const timestamp = new Date().toISOString();
    for (const line of message.split(/\r?\n/)) {
      this.recentLogs.push(`${timestamp} [${level}] ${line}`);
      if (this.recentLogs.length > MAX_RECENT_LOGS) {
        this.recentLogs.shift();
      }
    }
    switch (level) {
      case "INFO":
        this.outputChannel.info(message);
        break;
      case "WARN":
        this.outputChannel.warn(message);
        break;
      case "ERROR":
        this.outputChannel.error(message);
        break;
      case "DEBUG":
        this.outputChannel.debug(message);
        break;
    }
  }

  logInfo(message: string): void {
    this.pushLog("INFO", message);
  }

  logWarn(message: string): void {
    this.pushLog("WARN", message);
  }

  logError(message: string): void {
    this.pushLog("ERROR", message);
  }

  private recordDiagnostic(context: string, diagnostic: HaltdDiagnostic): void {
    this.lastDiagnostic = {
      at: new Date().toISOString(),
      context,
      diagnostic,
    };
  }

  private extractDiagnostic(error: unknown, context: string): HaltdDiagnostic {
    if (error instanceof HaltdClientError) {
      return error.diagnostic;
    }
    if (error instanceof BackendRpcError && error.haltd && isHaltdDiagnostic(error.haltd)) {
      return error.haltd;
    }
    if (isHaltdDiagnostic(error)) {
      return error;
    }

    const message = error instanceof Error ? error.message : String(error);
    const now = Date.now();
    while (this.recentDapErrors.length > 0 && now - this.recentDapErrors[0].at > DAP_ERROR_TTL_MS) {
      this.recentDapErrors.shift();
    }
    for (let i = this.recentDapErrors.length - 1; i >= 0; i--) {
      const entry = this.recentDapErrors[i];
      if (message === entry.diagnostic.message || message.includes(entry.diagnostic.message)) {
        return entry.diagnostic;
      }
    }

    const lower = message.toLowerCase();
    if (lower.includes("haltd backend is not connected") || lower.includes("connection lost")) {
      return {
        id: 2003,
        code: "E_WS_DISCONNECTED",
        message,
        detail: `context=${context}`,
        causes: [
          "后台 WebSocket 数据通道尚未建立或已断开（通常需要先启动一次调试会话）",
          "后台守护进程已退出，或 47110 端口存在冲突",
        ],
        actions: ["先按 F5 启动调试会话，或查看 Haltd 日志通道与 serve.log"],
      };
    }
    if (lower.includes("cannot use hss")) {
      return {
        id: 2004,
        code: "E_HSS_CAPACITY",
        message,
        detail: `context=${context}`,
        causes: [
          "所选表达式不是全局/静态标量，或探针 HSS 通道/字节预算已满",
        ],
        actions: [
          "确认表达式指向全局/静态变量并能静态寻址",
          "在 Timeline 网页端用 auto 模式添加（可自动降级为轮询采样）",
        ],
      };
    }
    if (lower.includes("build task") || lower.includes("build failed")) {
      return {
        id: 2005,
        code: "E_PRELAUNCH_BUILD",
        message,
        detail: `context=${context}`,
        causes: ["launch.json 中 preLaunchTask 名称不存在，或编译过程返回了非零退出码"],
        actions: ["检查 .vscode/tasks.json 任务名与终端编译输出"],
      };
    }
    if (lower.includes("device list is unavailable")) {
      return {
        id: 2006,
        code: "E_DEVICE_LIST",
        message,
        detail: `context=${context}`,
        causes: ["haltd 无法加载 J-Link 器件库，且内置备用列表未能读取"],
        actions: ["检查 J-Link Software 安装以及 haltd.path 设置"],
      };
    }
    return {
      id: 2099,
      code: "E_INTERNAL",
      message,
      detail: `context=${context}`,
      causes: ["操作执行遇到未分类错误，请结合完整日志排查"],
      actions: ["打开 Haltd 输出日志或运行 \"Haltd: Collect Diagnostics\""],
    };
  }

  private formatDiagnosticDetails(diag: HaltdDiagnostic, context: string): string {
    const lines = [
      `[${diag.code}] (${context}) ${diag.message}`,
    ];
    if (diag.detail) {
      lines.push(`  detail: ${diag.detail}`);
    }
    if (diag.causes.length > 0) {
      lines.push("  可能原因:");
      for (const cause of diag.causes) {
        lines.push(`    - ${cause}`);
      }
    }
    if (diag.actions.length > 0) {
      lines.push("  建议操作:");
      for (const action of diag.actions) {
        lines.push(`    - ${action}`);
      }
    }
    return lines.join("\n");
  }

  private async showDiagnosticToast(diag: HaltdDiagnostic, context: string): Promise<void> {
    const firstCause = diag.causes[0] ? `（可能原因：${diag.causes[0]}）` : "";
    const text = `Haltd [${diag.code}]: ${diag.message}${firstCause}`;
    const choice = await vscode.window.showErrorMessage(
      text,
      "查看详情",
      "打开日志",
      "复制诊断信息",
    );
    if (choice === "查看详情") {
      this.outputChannel.show(true);
    } else if (choice === "打开日志") {
      this.outputChannel.show(true);
      const serveLog = findServeLogPath();
      if (serveLog) {
        try {
          const doc = await vscode.workspace.openTextDocument(vscode.Uri.file(serveLog));
          await vscode.window.showTextDocument(doc, { preview: true });
        } catch {
          this.logWarn(`无法打开后台日志文件: ${serveLog}`);
        }
      } else {
        this.logInfo("未检测到后台 serve.log 文件（守护进程尚未写盘）");
      }
    } else if (choice === "复制诊断信息") {
      const report = await this.collectDiagnosticsMarkdown();
      await vscode.env.clipboard.writeText(report);
      void vscode.window.showInformationMessage("Haltd: 已将诊断信息复制到剪贴板");
    }
  }

  async reportError(error: unknown, context = "Command"): Promise<void> {
    const diag = this.extractDiagnostic(error, context);
    this.recordDiagnostic(context, diag);
    this.logError(this.formatDiagnosticDetails(diag, context));
    await this.showDiagnosticToast(diag, context);
  }

  recordWsClose(info: WsCloseInfo): void {
    this.lastWsClose = { ...info, at: new Date().toISOString() };
    if (info.expected) {
      this.logInfo(`WebSocket 数据通道已按预期关闭 (code=${info.code})`);
      return;
    }
    const { cause, action } = wsCloseExplanation(info);
    this.logWarn(`WebSocket 意外断开: ${cause}。建议：${action}`);
  }

  recordFault(fault: unknown): void {
    this.logError(`Target fault captured:\n${JSON.stringify(fault, null, 2)}`);
  }

  private checkAdapterVersion(adapter: string): string | undefined {
    try {
      let mtimeMs = 0;
      if (fs.existsSync(adapter)) {
        mtimeMs = fs.statSync(adapter).mtimeMs;
      }
      if (
        this.versionCache &&
        this.versionCache.adapter === adapter &&
        this.versionCache.mtimeMs === mtimeMs
      ) {
        return this.versionCache.versionOutput;
      }
      const versionOutput = execFileSync(adapter, ["--version"], {
        timeout: 2000,
        encoding: "utf8",
      }).trim();
      this.versionCache = { adapter, mtimeMs, versionOutput };
      return versionOutput;
    } catch {
      return undefined;
    }
  }

  /**
   * Preflight checks run after VS Code substitutes `${workspaceFolder}` etc.,
   * immediately before spawning the debug adapter.
   */
  preflightLaunch(config: vscode.DebugConfiguration): boolean {
    const adapter = this.getAdapterPath();
    const looksLikePath =
      path.isAbsolute(adapter) || adapter.includes(path.sep) || adapter.includes("/");
    if (looksLikePath) {
      if (!fs.existsSync(adapter)) {
        void this.reportError(
          new HaltdClientError({
            id: 2001,
            code: "E_ADAPTER_EXEC",
            message: `找不到 haltd 可执行文件: ${adapter}`,
            detail: `haltd.path="${adapter}"`,
            causes: [
              "扩展内置二进制缺失，或 settings.json 中的 \"haltd.path\" 指向了不存在的路径",
            ],
            actions: ["检查 \"haltd.path\" 设置，或执行 cargo install --path bins/haltd"],
          }),
          "Launch Preflight",
        );
        return false;
      }
      if (process.platform !== "win32") {
        try {
          fs.accessSync(adapter, fs.constants.X_OK);
        } catch {
          void this.reportError(
            new HaltdClientError({
              id: 2001,
              code: "E_ADAPTER_EXEC",
              message: `haltd 文件缺少可执行权限: ${adapter}`,
              detail: `chmod +x ${adapter}`,
              causes: ["文件权限位未包含可执行权限（+x）"],
              actions: [`在终端运行 chmod +x "${adapter}" 后重试`],
            }),
            "Launch Preflight",
          );
          return false;
        }
      }
    }

    const versionOut = this.checkAdapterVersion(adapter);
    if (!versionOut) {
      void this.reportError(
        new HaltdClientError({
          id: 2001,
          code: "E_ADAPTER_EXEC",
          message: `无法执行 "${adapter} --version"`,
          detail: `adapter="${adapter}"`,
          causes: [
            "找不到 haltd 命令（未加入 PATH 且扩展包内/~/.cargo/bin 中均未找到）",
            "或可执行文件已损坏、架构不匹配、缺少运行所需共享库",
          ],
          actions: ["配置 \"haltd.path\" 指向有效的 haltd 可执行文件"],
        }),
        "Launch Preflight",
      );
      return false;
    }

    const extVersion = String(this.context.extension.packageJSON.version ?? "");
    const binVersion = parseSemverFromOutput(versionOut);
    if (extVersion && binVersion && extVersion !== binVersion) {
      this.logWarn(
        `版本不一致: VS Code 扩展为 ${extVersion}，但 ${adapter} 版本为 ${binVersion}`,
      );
    }

    if (typeof config.device !== "string" || config.device.trim() === "") {
      void this.reportError(
        new HaltdClientError({
          id: 1011,
          code: "E_CONFIG",
          message: "launch.json 缺少目标芯片 \"device\" 配置",
          causes: ["调试配置未填写 \"device\" 字段，J-Link 无法识别目标型号"],
          actions: ["运行命令 \"Haltd: Select Device\" 自动填入目标芯片"],
        }),
        "Launch Preflight",
      );
      return false;
    }

    const elf = typeof config.elf === "string" ? config.elf.trim() : "";
    if (!elf) {
      const hasProgram = typeof config.program === "string" && config.program.trim() !== "";
      void this.reportError(
        new HaltdClientError({
          id: 1004,
          code: "E_ELF",
          message: "launch.json 缺少 \"elf\" 配置",
          causes: hasProgram
            ? [
                "配置中使用了 \"program\" 字段，但 Haltd 使用 \"elf\" 指定固件 ELF 文件路径",
              ]
            : ["调试配置未填写 \"elf\" 字段，无法加载调试符号与烧录固件"],
          actions: [
            "在 launch.json 中设置 \"elf\": \"${workspaceFolder}/build/firmware.elf\"",
          ],
        }),
        "Launch Preflight",
      );
      return false;
    }

    if (!fs.existsSync(elf)) {
      if (config.preLaunchTask) {
        this.logWarn(
          `启动前未找到 ELF (${elf})，将等待 preLaunchTask "${String(config.preLaunchTask)}" 构建生成`,
        );
      } else {
        void this.reportError(
          new HaltdClientError({
            id: 1004,
            code: "E_ELF",
            message: `找不到 ELF 文件: ${elf}`,
            detail: `elf="${elf}"`,
            causes: [
              "固件尚未编译生成，或 launch.json 中的 \"elf\" 路径拼写有误",
            ],
            actions: [
              "先构建固件，或检查 launch.json 中 \"elf\" 路径与 preLaunchTask 配置",
            ],
          }),
          "Launch Preflight",
        );
        return false;
      }
    }

    this.lastLaunchConfig = sanitizeConfig(config);
    return true;
  }

  createTrackerFactory(): vscode.DebugAdapterTrackerFactory {
    return {
      createDebugAdapterTracker: (session) => {
        this.logInfo(`开始调试会话 "${session.name}" (${session.configuration.request ?? "launch"})`);
        let shuttingDown = false;
        let exitedCleanly = false;
        return {
          onWillReceiveMessage: (message: unknown) => {
            if (!message || typeof message !== "object") {
              return;
            }
            const msg = message as { type?: string; command?: string; arguments?: Record<string, unknown> };
            if (msg.type === "request" && msg.command) {
              if (msg.command === "terminate" || msg.command === "disconnect") {
                shuttingDown = true;
              }
              if ((msg.command === "launch" || msg.command === "attach") && msg.arguments) {
                this.lastLaunchConfig = sanitizeConfig(msg.arguments);
              }
              if (!NOISY_DAP_COMMANDS.has(msg.command)) {
                this.pushLog("DEBUG", `DAP -> ${msg.command}`);
              }
            }
          },
          onDidSendMessage: (message: unknown) => {
            if (!message || typeof message !== "object") {
              return;
            }
            const msg = message as {
              type?: string;
              event?: string;
              command?: string;
              success?: boolean;
              message?: string;
              body?: { haltd?: unknown };
            };
            if (msg.type === "event" && msg.event === "terminated") {
              shuttingDown = true;
            }
            if (msg.type === "response" && msg.success === false) {
              const command = msg.command ?? "unknown";
              const diag = isHaltdDiagnostic(msg.body?.haltd)
                ? msg.body.haltd
                : this.extractDiagnostic(msg.message ?? "DAP request failed", `DAP ${command}`);
              this.recentDapErrors.push({ command, at: Date.now(), diagnostic: diag });
              if (this.recentDapErrors.length > 32) {
                this.recentDapErrors.shift();
              }
              this.recordDiagnostic(`DAP ${command}`, diag);
              this.logError(this.formatDiagnosticDetails(diag, `DAP ${command}`));
              if (
                command === "launch" ||
                command === "attach" ||
                command === "configurationDone"
              ) {
                void this.showDiagnosticToast(diag, `DAP ${command}`);
              }
            }
          },
          onError: (error: Error) => {
            if (shuttingDown || exitedCleanly) {
              this.pushLog("DEBUG", `Debug adapter pipe closed during shutdown: ${error.message}`);
              return;
            }
            this.logError(`Debug adapter transport error: ${error.message}`);
            void this.reportError(error, "DebugAdapter");
          },
          onExit: (code: number | undefined, signal: string | undefined) => {
            this.logInfo(`Debug adapter exited (code=${code ?? "null"}, signal=${signal ?? "none"})`);
            if ((code === undefined || code === 0) && !signal) {
              exitedCleanly = true;
            }
            if ((code !== undefined && code !== 0) || signal) {
              const diag = adapterExitDiagnostic(code, signal, this.getAdapterPath());
              this.recordDiagnostic("DebugAdapter Exit", diag);
              this.logError(this.formatDiagnosticDetails(diag, "DebugAdapter Exit"));
              void this.showDiagnosticToast(diag, "DebugAdapter Exit");
            }
          },
        };
      },
    };
  }

  async collectDiagnosticsMarkdown(): Promise<string> {
    const extVersion = String(this.context.extension.packageJSON.version ?? "unknown");
    const adapter = this.getAdapterPath();
    const adapterVersion = this.checkAdapterVersion(adapter) ?? "<unavailable>";

    let backendStatus: unknown = this.client.connected ? "connected (status unavailable)" : "disconnected";
    if (this.client.connected) {
      try {
        backendStatus = await Promise.race([
          this.client.call("session.status"),
          new Promise<string>((resolve) => setTimeout(() => resolve("timeout"), 1000)),
        ]);
      } catch (err) {
        backendStatus = `error: ${err instanceof Error ? err.message : String(err)}`;
      }
    }

    const serveLogPath = findServeLogPath();
    const serveLogTail = serveLogPath ? readLogTail(serveLogPath, SERVE_LOG_TAIL_LINES) : undefined;

    const sections: string[] = [
      "# Haltd Diagnostics Report",
      "",
      `- **Generated**: ${new Date().toISOString()}`,
      `- **Extension Version**: ${extVersion}`,
      `- **VS Code Version**: ${vscode.version}`,
      `- **Platform**: ${process.platform} ${process.arch} (${os.release()})`,
      `- **Adapter Path**: \`${sanitizeValue(adapter, []) as string}\``,
      `- **Adapter --version**: \`${adapterVersion}\``,
      `- **WebSocket Status**: \`${this.client.connected ? "connected" : "disconnected"}\``,
      "",
      "## Backend Session Status",
      "```json",
      JSON.stringify(backendStatus, null, 2),
      "```",
    ];

    if (this.lastLaunchConfig) {
      sections.push(
        "",
        "## Last Launch Configuration (Sanitized)",
        "```json",
        JSON.stringify(this.lastLaunchConfig, null, 2),
        "```",
      );
    }

    if (this.lastDiagnostic) {
      sections.push(
        "",
        "## Last Structured Diagnostic",
        `- **Time**: ${this.lastDiagnostic.at}`,
        `- **Context**: ${this.lastDiagnostic.context}`,
        "```json",
        JSON.stringify(this.lastDiagnostic.diagnostic, null, 2),
        "```",
      );
    }

    if (this.lastWsClose) {
      sections.push(
        "",
        "## Last WebSocket Close",
        "```json",
        JSON.stringify(this.lastWsClose, null, 2),
        "```",
      );
    }

    if (serveLogPath) {
      sections.push(
        "",
        `## Backend serve.log (Last ${SERVE_LOG_TAIL_LINES} Lines)`,
        `Path: \`${sanitizeValue(serveLogPath, []) as string}\``,
        "```text",
        serveLogTail ?? "<unreadable>",
        "```",
      );
    }

    sections.push(
      "",
      `## Extension Log Buffer (Last ${this.recentLogs.length} Lines)`,
      "```text",
      this.recentLogs.length > 0 ? this.recentLogs.join("\n") : "<empty>",
      "```",
      "",
    );

    return sections.join("\n");
  }

  async openDiagnosticsReport(): Promise<void> {
    const content = await this.collectDiagnosticsMarkdown();
    const doc = await vscode.workspace.openTextDocument({
      language: "markdown",
      content,
    });
    await vscode.window.showTextDocument(doc, { preview: false });
  }

  dispose(): void {
    this.outputChannel.dispose();
  }
}
