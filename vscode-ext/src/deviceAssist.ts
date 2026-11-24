import { execFile } from "child_process";
import * as fs from "fs";
import * as path from "path";
import * as vscode from "vscode";

export interface DeviceEntry {
  name: string;
  manufacturer: string;
  core: string;
  flashSize: number;
  ramSize: number;
}

interface ParsedDeviceConfig {
  name: string;
  objectStart: number;
  typeIsJlinkd: boolean;
  deviceValue?: string;
  deviceValueRange?: vscode.Range;
  deviceQuoteRange?: vscode.Range;
  typeLineRange?: vscode.Range;
}

const LAUNCH_SELECTOR: vscode.DocumentSelector = [
  { language: "jsonc", pattern: "**/.vscode/launch.json" },
  { language: "json", pattern: "**/.vscode/launch.json" },
];

const CLI_TIMEOUT_MS = 5000;

function formatKiB(bytes: number): string {
  if (bytes === 0) {
    return "0 KiB";
  }
  const kib = Math.round(bytes / 1024);
  if (kib >= 1024 && kib % 1024 === 0) {
    return `${kib / 1024} MiB`;
  }
  return `${kib} KiB`;
}

/** Mirror `family_for_device` from `crates/engine/src/project.rs`. */
export function profileForDevice(device: string): string | undefined {
  const upper = device.toUpperCase();
  if (upper.startsWith("STM32H74") || upper.startsWith("STM32H75")) {
    return "stm32h74";
  }
  if (upper.startsWith("STM32H7")) {
    return "stm32h7";
  }
  if (upper.startsWith("STM32G4")) {
    return "stm32g4";
  }
  if (upper.startsWith("STM32F1")) {
    return "stm32f1";
  }
  return undefined;
}

function execDevicesCli(executable: string): Promise<DeviceEntry[]> {
  return new Promise((resolve, reject) => {
    execFile(
      executable,
      ["devices", "--json"],
      { timeout: CLI_TIMEOUT_MS, maxBuffer: 16 * 1024 * 1024 },
      (error, stdout) => {
        if (error) {
          reject(error);
          return;
        }
        try {
          const parsed = JSON.parse(stdout) as unknown;
          if (!Array.isArray(parsed) || parsed.length === 0) {
            reject(new Error("empty device list from jlinkd"));
            return;
          }
          resolve(parsed as DeviceEntry[]);
        } catch (parseErr) {
          reject(parseErr);
        }
      },
    );
  });
}

class DeviceCatalog {
  private cached: Promise<DeviceEntry[]> | undefined;
  private byUpperName: Map<string, DeviceEntry> | undefined;

  constructor(
    private readonly extensionPath: string,
    private readonly getAdapterPath: () => string,
  ) {}

  async list(): Promise<DeviceEntry[]> {
    this.cached ??= this.load();
    return this.cached;
  }

  async lookup(name: string): Promise<DeviceEntry | undefined> {
    if (!this.byUpperName) {
      const entries = await this.list();
      const map = new Map<string, DeviceEntry>();
      for (const entry of entries) {
        map.set(entry.name.toUpperCase(), entry);
      }
      this.byUpperName = map;
    }
    return this.byUpperName.get(name.toUpperCase());
  }

  private async load(): Promise<DeviceEntry[]> {
    try {
      return await execDevicesCli(this.getAdapterPath());
    } catch {
      const fallback = path.join(this.extensionPath, "resources", "devices-stm32.json");
      try {
        const raw = fs.readFileSync(fallback, "utf8");
        return JSON.parse(raw) as DeviceEntry[];
      } catch {
        return [];
      }
    }
  }
}

interface JsoncToken {
  kind: "string" | "punct" | "other";
  text: string;
  start: number;
  end: number;
}

function tokenizeJsonc(source: string): JsoncToken[] {
  const tokens: JsoncToken[] = [];
  let i = 0;
  const len = source.length;
  while (i < len) {
    const ch = source[i];
    if (ch === " " || ch === "\t" || ch === "\r" || ch === "\n") {
      i++;
      continue;
    }
    if (ch === "/" && source[i + 1] === "/") {
      i += 2;
      while (i < len && source[i] !== "\n") {
        i++;
      }
      continue;
    }
    if (ch === "/" && source[i + 1] === "*") {
      i += 2;
      while (i + 1 < len && !(source[i] === "*" && source[i + 1] === "/")) {
        i++;
      }
      i = Math.min(len, i + 2);
      continue;
    }
    if (ch === '"') {
      const start = i;
      i++;
      let value = "";
      while (i < len) {
        const c = source[i];
        if (c === "\\") {
          if (i + 1 < len) {
            value += source[i + 1];
            i += 2;
            continue;
          }
          i++;
          break;
        }
        if (c === '"') {
          i++;
          break;
        }
        if (c === "\n" || c === "\r") {
          break;
        }
        value += c;
        i++;
      }
      tokens.push({ kind: "string", text: value, start, end: i });
      continue;
    }
    if (ch === "{" || ch === "}" || ch === "[" || ch === "]" || ch === ":" || ch === ",") {
      tokens.push({ kind: "punct", text: ch, start: i, end: i + 1 });
      i++;
      continue;
    }
    const start = i;
    while (i < len && !' \t\r\n{}[]:,"'.includes(source[i]) && !(source[i] === "/" && (source[i + 1] === "/" || source[i + 1] === "*"))) {
      i++;
    }
    tokens.push({ kind: "other", text: source.slice(start, i), start, end: i });
  }
  return tokens;
}

/** Find every object literal in launch.json that has `"type": "jlinkd"`. */
function parseJlinkdConfigs(document: vscode.TextDocument): ParsedDeviceConfig[] {
  const tokens = tokenizeJsonc(document.getText());
  const configs: ParsedDeviceConfig[] = [];
  const stack: Array<{
    depth: number;
    start: number;
    name: string;
    typeValue?: string;
    typeLineRange?: vscode.Range;
    deviceValue?: string;
    deviceValueRange?: vscode.Range;
    deviceQuoteRange?: vscode.Range;
  }> = [];

  for (let i = 0; i < tokens.length; i++) {
    const tok = tokens[i];
    if (tok.kind === "punct" && tok.text === "{") {
      stack.push({ depth: stack.length + 1, start: tok.start, name: "jlinkd" });
      continue;
    }
    if (tok.kind === "punct" && tok.text === "}") {
      const top = stack.pop();
      if (top && top.typeValue === "jlinkd") {
        configs.push({
          name: top.name,
          objectStart: top.start,
          typeIsJlinkd: true,
          deviceValue: top.deviceValue,
          deviceValueRange: top.deviceValueRange,
          deviceQuoteRange: top.deviceQuoteRange,
          typeLineRange: top.typeLineRange,
        });
      }
      continue;
    }
    if (stack.length === 0 || tok.kind !== "string") {
      continue;
    }
    const colon = tokens[i + 1];
    const val = tokens[i + 2];
    if (!colon || colon.kind !== "punct" || colon.text !== ":" || !val) {
      continue;
    }
    // Only record keys belonging to the innermost open object when it wasn't pushed by a nested array/object.
    const current = stack[stack.length - 1];
    const key = tok.text;
    if (key === "type" && val.kind === "string") {
      current.typeValue = val.text;
      current.typeLineRange = new vscode.Range(
        document.positionAt(tok.start),
        document.positionAt(val.end),
      );
    } else if (key === "name" && val.kind === "string") {
      current.name = val.text;
    } else if (key === "device" && val.kind === "string") {
      current.deviceValue = val.text;
      const innerStart = Math.min(val.start + 1, val.end);
      const innerEnd = Math.max(innerStart, val.end - (document.getText(new vscode.Range(document.positionAt(val.end - 1), document.positionAt(val.end))) === '"' ? 1 : 0));
      current.deviceValueRange = new vscode.Range(
        document.positionAt(innerStart),
        document.positionAt(innerEnd),
      );
      current.deviceQuoteRange = new vscode.Range(
        document.positionAt(val.start),
        document.positionAt(val.end),
      );
    }
  }
  return configs;
}

/** Determine whether the cursor sits inside a `"device": "..."` value of a jlinkd config,
 *  tolerating incomplete JSON while typing. */
function completionRangeAt(
  document: vscode.TextDocument,
  position: vscode.Position,
): vscode.Range | undefined {
  const line = document.lineAt(position.line).text;
  const before = line.slice(0, position.character);
  const match = before.match(/"device"\s*:\s*"([^"]*)$/);
  if (!match) {
    return undefined;
  }
  const valueStartCol = position.character - match[1].length;
  const after = line.slice(position.character);
  const afterMatch = after.match(/^([^"]*)"/);
  const valueEndCol = afterMatch ? position.character + afterMatch[1].length : position.character;

  // Verify the enclosing object has `"type": "jlinkd"`.
  const text = document.getText();
  const offset = document.offsetAt(position);
  let depth = 0;
  let openBrace = -1;
  for (let i = offset - 1; i >= 0; i--) {
    const ch = text[i];
    if (ch === "}") {
      depth++;
    } else if (ch === "{") {
      if (depth === 0) {
        openBrace = i;
        break;
      }
      depth--;
    }
  }
  if (openBrace < 0) {
    return undefined;
  }
  depth = 0;
  let closeBrace = text.length;
  for (let i = offset; i < text.length; i++) {
    const ch = text[i];
    if (ch === "{") {
      depth++;
    } else if (ch === "}") {
      if (depth === 0) {
        closeBrace = i + 1;
        break;
      }
      depth--;
    }
  }
  const block = text.slice(openBrace, closeBrace);
  if (!/"type"\s*:\s*"jlinkd"/.test(block)) {
    return undefined;
  }
  return new vscode.Range(position.line, valueStartCol, position.line, valueEndCol);
}

function deviceMarkdown(entry: DeviceEntry): vscode.MarkdownString {
  const profile = profileForDevice(entry.name);
  const lines = [
    `**${entry.name}** (${entry.manufacturer || "Unknown"})`,
    "",
    `- Core: \`${entry.core}\``,
    `- Flash: ${formatKiB(entry.flashSize)} | RAM: ${formatKiB(entry.ramSize)}`,
    `- Profile: ${profile ? `\`devices/${profile}.yaml\`` : "no built-in profile"}`,
  ];
  return new vscode.MarkdownString(lines.join("\n"));
}

function isLaunchJson(document: vscode.TextDocument): boolean {
  const normalized = document.uri.fsPath.replace(/\\/g, "/");
  return normalized.endsWith("/.vscode/launch.json");
}

export function registerDeviceAssist(
  context: vscode.ExtensionContext,
  getAdapterPath: () => string,
): void {
  const catalog = new DeviceCatalog(context.extensionPath, getAdapterPath);
  const diagnostics = vscode.languages.createDiagnosticCollection("jlinkd");

  const validateDocument = async (document: vscode.TextDocument) => {
    if (!isLaunchJson(document)) {
      return;
    }
    const entries = await catalog.list();
    if (entries.length === 0) {
      diagnostics.delete(document.uri);
      return;
    }
    const configs = parseJlinkdConfigs(document);
    const issues: vscode.Diagnostic[] = [];
    for (const cfg of configs) {
      if (!cfg.deviceQuoteRange) {
        continue;
      }
      const value = cfg.deviceValue ?? "";
      if (value.trim() === "") {
        issues.push(
          new vscode.Diagnostic(
            cfg.deviceQuoteRange,
            "jlinkd: device is empty",
            vscode.DiagnosticSeverity.Warning,
          ),
        );
        continue;
      }
      // Skip unresolved VS Code variable placeholders like ${input:device}.
      if (value.includes("${")) {
        continue;
      }
      const found = await catalog.lookup(value);
      if (!found) {
        issues.push(
          new vscode.Diagnostic(
            cfg.deviceQuoteRange,
            `jlinkd: unknown J-Link device "${value}"`,
            vscode.DiagnosticSeverity.Warning,
          ),
        );
      }
    }
    diagnostics.set(document.uri, issues);
  };

  for (const doc of vscode.workspace.textDocuments) {
    void validateDocument(doc);
  }

  context.subscriptions.push(
    diagnostics,
    vscode.workspace.onDidOpenTextDocument((doc) => void validateDocument(doc)),
    vscode.workspace.onDidChangeTextDocument((event) => void validateDocument(event.document)),
    vscode.workspace.onDidSaveTextDocument((doc) => void validateDocument(doc)),
    vscode.workspace.onDidCloseTextDocument((doc) => diagnostics.delete(doc.uri)),
    vscode.languages.registerCompletionItemProvider(
      LAUNCH_SELECTOR,
      {
        async provideCompletionItems(document, position) {
          const range = completionRangeAt(document, position);
          if (!range) {
            return undefined;
          }
          const devices = await catalog.list();
          return devices.map((entry) => {
            const item = new vscode.CompletionItem(entry.name, vscode.CompletionItemKind.Value);
            item.detail = `${entry.core} · ${formatKiB(entry.flashSize)} Flash · ${formatKiB(entry.ramSize)} RAM`;
            item.documentation = deviceMarkdown(entry);
            item.range = range;
            item.insertText = entry.name;
            item.filterText = entry.name;
            item.sortText = entry.name.toUpperCase().startsWith("STM32")
              ? `0_${entry.name}`
              : `1_${entry.name}`;
            return item;
          });
        },
      },
      '"',
      ":",
    ),
    vscode.languages.registerHoverProvider(LAUNCH_SELECTOR, {
      async provideHover(document, position) {
        const configs = parseJlinkdConfigs(document);
        for (const cfg of configs) {
          if (cfg.deviceQuoteRange?.contains(position) && cfg.deviceValue) {
            const entry = await catalog.lookup(cfg.deviceValue);
            if (entry) {
              return new vscode.Hover(deviceMarkdown(entry), cfg.deviceQuoteRange);
            }
            const profile = profileForDevice(cfg.deviceValue);
            return new vscode.Hover(
              new vscode.MarkdownString(
                `**${cfg.deviceValue}**\n\nNot found in cached J-Link device list.\n\n- Profile: ${
                  profile ? `\`devices/${profile}.yaml\`` : "no built-in profile"
                }`,
              ),
              cfg.deviceQuoteRange,
            );
          }
        }
        return undefined;
      },
    }),
    vscode.commands.registerCommand("jlinkd.selectDevice", async () => {
      const targetDoc = await findLaunchDocument();
      if (!targetDoc) {
        void vscode.window.showWarningMessage("jlinkd: no .vscode/launch.json found in workspace");
        return;
      }
      const configs = parseJlinkdConfigs(targetDoc);
      if (configs.length === 0) {
        void vscode.window.showWarningMessage(
          'jlinkd: no configuration with "type": "jlinkd" found in launch.json',
        );
        return;
      }
      let chosenConfig = configs[0];
      if (configs.length > 1) {
        const picked = await vscode.window.showQuickPick(
          configs.map((cfg) => ({
            label: cfg.name,
            description: cfg.deviceValue ? `current: ${cfg.deviceValue}` : "no device set",
            cfg,
          })),
          { placeHolder: "Select launch configuration to update" },
        );
        if (!picked) {
          return;
        }
        chosenConfig = picked.cfg;
      }

      const devices = await catalog.list();
      if (devices.length === 0) {
        void vscode.window.showErrorMessage("jlinkd: device list is unavailable");
        return;
      }
      const ordered = [...devices].sort((a, b) => {
        const aStm = a.name.toUpperCase().startsWith("STM32") ? 0 : 1;
        const bStm = b.name.toUpperCase().startsWith("STM32") ? 0 : 1;
        return aStm - bStm || a.name.localeCompare(b.name);
      });
      const pickedDevice = await vscode.window.showQuickPick(
        ordered.map((entry) => {
          const profile = profileForDevice(entry.name);
          return {
            label: entry.name,
            description: `${entry.core} · ${formatKiB(entry.flashSize)} Flash · ${formatKiB(entry.ramSize)} RAM`,
            detail: profile ? `Profile: devices/${profile}.yaml` : entry.manufacturer,
            entry,
          };
        }),
        { placeHolder: "Select J-Link target device", matchOnDescription: true },
      );
      if (!pickedDevice) {
        return;
      }

      const edit = new vscode.WorkspaceEdit();
      if (chosenConfig.deviceValueRange) {
        edit.replace(targetDoc.uri, chosenConfig.deviceValueRange, pickedDevice.entry.name);
      } else if (chosenConfig.typeLineRange) {
        const line = targetDoc.lineAt(chosenConfig.typeLineRange.end.line);
        const indentMatch = line.text.match(/^(\s*)/);
        const indent = indentMatch ? indentMatch[1] : "      ";
        if (!line.text.trimEnd().endsWith(",")) {
          edit.insert(targetDoc.uri, line.range.end, ",");
        }
        edit.insert(
          targetDoc.uri,
          line.range.end,
          `\n${indent}"device": "${pickedDevice.entry.name}",`,
        );
      } else {
        void vscode.window.showErrorMessage("jlinkd: cannot locate position to insert device");
        return;
      }
      await vscode.workspace.applyEdit(edit);
      await vscode.window.showTextDocument(targetDoc);
    }),
  );
}

async function findLaunchDocument(): Promise<vscode.TextDocument | undefined> {
  const active = vscode.window.activeTextEditor?.document;
  if (active && isLaunchJson(active)) {
    return active;
  }
  const folder = vscode.workspace.workspaceFolders?.[0];
  if (!folder) {
    return undefined;
  }
  const candidate = vscode.Uri.joinPath(folder.uri, ".vscode", "launch.json");
  if (!fs.existsSync(candidate.fsPath)) {
    return undefined;
  }
  return vscode.workspace.openTextDocument(candidate);
}
