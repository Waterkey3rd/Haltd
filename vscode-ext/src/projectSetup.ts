import * as fs from "fs";
import * as path from "path";
import * as vscode from "vscode";
import { formatKiB, profileForDevice, type DeviceCatalog, type DeviceEntry } from "./deviceAssist";
import {
  detectDeviceHints,
  deviceMatchesHints,
  inferDefaultElfRelPath,
  isArmElfHeader,
  mergeHaltdLaunchJson,
  resolveOrGenerateBuildTask,
  type ProjectBuildFiles,
} from "./projectSetupCore";
import type { SvdCacheManager } from "./svdCache";

interface FoundElf {
  absPath: string;
  relPath: string;
  sizeBytes: number;
  mtimeMs: number;
  isDebug: boolean;
}

function formatSize(bytes: number): string {
  if (bytes >= 1024 * 1024) {
    return `${(bytes / (1024 * 1024)).toFixed(2)} MiB`;
  }
  return `${Math.max(1, Math.round(bytes / 1024))} KiB`;
}

function formatAge(mtimeMs: number): string {
  const diffSec = Math.max(0, Math.round((Date.now() - mtimeMs) / 1000));
  if (diffSec < 60) return "just now";
  if (diffSec < 3600) return `${Math.floor(diffSec / 60)}m ago`;
  if (diffSec < 86400) return `${Math.floor(diffSec / 3600)}h ago`;
  return `${Math.floor(diffSec / 86400)}d ago`;
}

function readTextIfExists(filePath: string, maxBytes = 128 * 1024): string | undefined {
  try {
    const stat = fs.statSync(filePath);
    if (!stat.isFile()) return undefined;
    if (stat.size <= maxBytes) {
      return fs.readFileSync(filePath, "utf8");
    }
    const fd = fs.openSync(filePath, "r");
    try {
      const buf = Buffer.alloc(maxBytes);
      const bytesRead = fs.readSync(fd, buf, 0, maxBytes, 0);
      return buf.subarray(0, bytesRead).toString("utf8");
    } finally {
      fs.closeSync(fd);
    }
  } catch {
    return undefined;
  }
}

async function pickWorkspaceFolder(): Promise<vscode.WorkspaceFolder | undefined> {
  const folders = vscode.workspace.workspaceFolders;
  if (!folders || folders.length === 0) {
    void vscode.window.showWarningMessage("Haltd: open a project folder before configuring debugging");
    return undefined;
  }
  if (folders.length === 1) {
    return folders[0];
  }
  return vscode.window.showWorkspaceFolderPick({
    placeHolder: "Select workspace folder to configure for Haltd",
  });
}

async function collectHintFiles(folder: vscode.WorkspaceFolder): Promise<Array<{ name: string; content: string }>> {
  const out: Array<{ name: string; content: string }> = [];
  const root = folder.uri.fsPath;
  for (const name of ["CMakeLists.txt", "Makefile", ".vscode/launch.json"]) {
    const content = readTextIfExists(path.join(root, name));
    if (content) {
      out.push({ name, content });
    }
  }
  const extraUris = await vscode.workspace.findFiles(
    new vscode.RelativePattern(folder, "{*.ioc,*.jdebug,*_FLASH.ld,*_flash.ld,cmake/*.cmake,Core/**/startup_*.s,startup_*.s}"),
    "**/{.git,node_modules,build,dist,.cache}/**",
    16,
  );
  for (const uri of extraUris) {
    const content = readTextIfExists(uri.fsPath);
    if (content !== undefined) {
      out.push({ name: path.basename(uri.fsPath), content });
    } else {
      out.push({ name: path.basename(uri.fsPath), content: "" });
    }
  }
  return out;
}

async function pickTargetDevice(
  folder: vscode.WorkspaceFolder,
  catalog: DeviceCatalog,
): Promise<DeviceEntry | undefined> {
  const devices = await catalog.list();
  if (devices.length === 0) {
    throw new Error("device list is unavailable");
  }
  const hints = detectDeviceHints(await collectHintFiles(folder));

  const ranked = [...devices].sort((a, b) => {
    const ma = deviceMatchesHints(a.name, hints);
    const mb = deviceMatchesHints(b.name, hints);
    const rankA = ma === "exact" ? 0 : ma === "prefix" ? 1 : a.name.toUpperCase().startsWith("STM32") ? 2 : 3;
    const rankB = mb === "exact" ? 0 : mb === "prefix" ? 1 : b.name.toUpperCase().startsWith("STM32") ? 2 : 3;
    return rankA - rankB || a.name.localeCompare(b.name);
  });

  const items = ranked.map((entry) => {
    const match = deviceMatchesHints(entry.name, hints);
    const profile = profileForDevice(entry.name);
    const hintPrefix = match ? "$(sparkle) Detected in project · " : "";
    return {
      label: entry.name,
      description: `${entry.core} · ${formatKiB(entry.flashSize)} Flash · ${formatKiB(entry.ramSize)} RAM`,
      detail: `${hintPrefix}${profile ? `Profile: devices/${profile}.yaml` : entry.manufacturer || "Generic"}`,
      entry,
    };
  });

  const picked = await vscode.window.showQuickPick(items, {
    placeHolder: "Step 1/2: Select target MCU (e.g. STM32H723VG, STM32G474RE, STM32F103C8)",
    matchOnDescription: true,
    matchOnDetail: true,
  });
  return picked?.entry;
}

function readArmElfInfo(root: string, absPath: string): FoundElf | undefined {
  try {
    const stat = fs.statSync(absPath);
    if (!stat.isFile() || stat.size < 64) {
      return undefined;
    }
    const fd = fs.openSync(absPath, "r");
    try {
      const header = Buffer.alloc(20);
      const read = fs.readSync(fd, header, 0, 20, 0);
      if (read < 20 || !isArmElfHeader(header)) {
        return undefined;
      }
    } finally {
      fs.closeSync(fd);
    }
    const relPath = path.relative(root, absPath).replace(/\\/g, "/");
    return {
      absPath,
      relPath,
      sizeBytes: stat.size,
      mtimeMs: stat.mtimeMs,
      isDebug: /(^|\/)debug(\/|$)/i.test(relPath),
    };
  } catch {
    return undefined;
  }
}

function readProjectBuildFiles(root: string, hints: Array<{ name: string; content: string }>): ProjectBuildFiles {
  const ioc = hints.find((f) => f.name.toLowerCase().endsWith(".ioc"))?.content;
  const hasNinja =
    fs.existsSync(path.join(root, "build", "build.ninja")) ||
    fs.existsSync(path.join(root, "build", "Debug", "build.ninja"));
  return {
    cmakeLists: readTextIfExists(path.join(root, "CMakeLists.txt")),
    cmakePresets: readTextIfExists(path.join(root, "CMakePresets.json")),
    makefile: readTextIfExists(path.join(root, "Makefile")),
    ioc,
    hasNinjaBuild: hasNinja,
  };
}

async function resolveElfPath(
  folder: vscode.WorkspaceFolder,
  buildFiles: ProjectBuildFiles,
): Promise<{ elfLaunchProp: string; relPath: string } | undefined> {
  const root = folder.uri.fsPath;
  const uris = await vscode.workspace.findFiles(
    new vscode.RelativePattern(folder, "**/*.{elf,axf}"),
    "**/{.git,node_modules,.cache,.vscode,release,dist}/**",
    64,
  );

  const valid = uris
    .map((uri) => readArmElfInfo(root, uri.fsPath))
    .filter((item): item is FoundElf => Boolean(item))
    .sort((a, b) => {
      if (a.isDebug !== b.isDebug) {
        return a.isDebug ? -1 : 1;
      }
      return b.mtimeMs - a.mtimeMs || a.relPath.localeCompare(b.relPath);
    });

  if (valid.length === 1) {
    return {
      elfLaunchProp: `\${workspaceFolder}/${valid[0].relPath}`,
      relPath: valid[0].relPath,
    };
  }

  if (valid.length > 1) {
    const picked = await vscode.window.showQuickPick(
      valid.map((item) => ({
        label: item.relPath,
        description: `${formatSize(item.sizeBytes)} · updated ${formatAge(item.mtimeMs)}`,
        detail: item.isDebug ? "Debug build output" : item.absPath,
        item,
      })),
      {
        placeHolder: `Step 2/2: Found ${valid.length} ARM ELF files — select the firmware image to debug`,
        matchOnDescription: true,
      },
    );
    if (!picked) {
      return undefined;
    }
    return {
      elfLaunchProp: `\${workspaceFolder}/${picked.item.relPath}`,
      relPath: picked.item.relPath,
    };
  }

  const inferred = inferDefaultElfRelPath(buildFiles);
  const entered = await vscode.window.showInputBox({
    title: "Haltd: Firmware ELF Path",
    prompt: "No compiled ARM .elf found yet. Confirm the expected build output path:",
    value: `\${workspaceFolder}/${inferred}`,
    ignoreFocusOut: true,
  });
  if (!entered || !entered.trim()) {
    return undefined;
  }
  const trimmed = entered.trim();
  const relPath = trimmed.startsWith("${workspaceFolder}/")
    ? trimmed.slice("${workspaceFolder}/".length)
    : trimmed;
  return {
    elfLaunchProp: trimmed,
    relPath,
  };
}

async function resolveBuildTask(
  folder: vscode.WorkspaceFolder,
  elfRelPath: string,
  buildFiles: ProjectBuildFiles,
): Promise<{ preLaunchTask?: string; generatedTasksLabel?: string }> {
  const vscodeDir = path.join(folder.uri.fsPath, ".vscode");
  const tasksPath = path.join(vscodeDir, "tasks.json");
  const existingTasks = readTextIfExists(tasksPath);
  const decision = resolveOrGenerateBuildTask(existingTasks, elfRelPath, buildFiles);

  if (decision.defaultLabel && !decision.generatedTask) {
    return { preLaunchTask: decision.defaultLabel };
  }
  if (decision.existingLabels.length > 1) {
    const picked = await vscode.window.showQuickPick(
      [
        ...decision.existingLabels.map((label) => ({ label, description: "Existing build task" })),
        { label: "$(circle-slash) None", description: "Do not run a preLaunchTask" },
      ],
      { placeHolder: "Select the preLaunchTask for Build & Debug (F5 / F6)" },
    );
    if (picked && !picked.label.includes("None")) {
      return { preLaunchTask: picked.label };
    }
    return {};
  }
  if (decision.generatedTask) {
    fs.mkdirSync(vscodeDir, { recursive: true });
    fs.writeFileSync(tasksPath, decision.generatedTask.tasksJsonContent, "utf8");
    return {
      preLaunchTask: decision.generatedTask.label,
      generatedTasksLabel: decision.generatedTask.label,
    };
  }
  return {};
}

export async function runConfigureProject(
  catalog: DeviceCatalog,
  svdCache: SvdCacheManager,
  log: (msg: string) => void,
  targetFolder?: vscode.WorkspaceFolder,
): Promise<boolean> {
  const folder = targetFolder ?? (await pickWorkspaceFolder());
  if (!folder) {
    return false;
  }

  const device = await pickTargetDevice(folder, catalog);
  if (!device) {
    return false;
  }

  const hintFiles = await collectHintFiles(folder);
  const buildFiles = readProjectBuildFiles(folder.uri.fsPath, hintFiles);
  const elfChoice = await resolveElfPath(folder, buildFiles);
  if (!elfChoice) {
    return false;
  }

  const { preLaunchTask, generatedTasksLabel } = await resolveBuildTask(
    folder,
    elfChoice.relPath,
    buildFiles,
  );

  const vscodeDir = path.join(folder.uri.fsPath, ".vscode");
  const launchPath = path.join(vscodeDir, "launch.json");
  const existingLaunch = readTextIfExists(launchPath);
  const updatedLaunch = mergeHaltdLaunchJson(existingLaunch, {
    device: device.name,
    elf: elfChoice.elfLaunchProp,
    preLaunchTask,
  });

  fs.mkdirSync(vscodeDir, { recursive: true });
  fs.writeFileSync(launchPath, updatedLaunch, "utf8");

  if (svdCache.autoDownloadEnabled()) {
    void svdCache
      .ensureForDevice(device.name, { manufacturer: device.manufacturer })
      .then((svdPath) => {
        if (svdPath) {
          log(`SVD ready for ${device.name}: ${svdPath}`);
        }
      });
  }

  const doc = await vscode.workspace.openTextDocument(vscode.Uri.file(launchPath));
  await vscode.window.showTextDocument(doc, { preview: false });

  const taskNote = generatedTasksLabel
    ? ` · generated task "${generatedTasksLabel}"`
    : preLaunchTask
      ? ` · task "${preLaunchTask}"`
      : "";
  const choice = await vscode.window.showInformationMessage(
    `Haltd: configured ${device.name} (${elfChoice.relPath}${taskNote}) with Build & Debug + Attach`,
    "Start Debugging (F5)",
  );
  if (choice === "Start Debugging (F5)") {
    await vscode.debug.startDebugging(folder, "Haltd: Build & Debug");
  }
  return true;
}

/**
 * Used by `DebugConfigurationProvider.provideDebugConfigurations` when VS Code asks
 * to populate a new `launch.json`.
 */
export async function provideWizardConfigurations(
  folder: vscode.WorkspaceFolder | undefined,
  catalog: DeviceCatalog,
  svdCache: SvdCacheManager,
  log: (msg: string) => void,
): Promise<vscode.DebugConfiguration[]> {
  if (folder) {
    const device = await pickTargetDevice(folder, catalog);
    if (device) {
      const hintFiles = await collectHintFiles(folder);
      const buildFiles = readProjectBuildFiles(folder.uri.fsPath, hintFiles);
      const elfChoice = await resolveElfPath(folder, buildFiles);
      if (elfChoice) {
        const { preLaunchTask } = await resolveBuildTask(folder, elfChoice.relPath, buildFiles);
        if (svdCache.autoDownloadEnabled()) {
          void svdCache
            .ensureForDevice(device.name, { manufacturer: device.manufacturer })
            .then((svdPath) => {
              if (svdPath) log(`SVD ready for ${device.name}: ${svdPath}`);
            });
        }
        return [
          {
            name: "Haltd: Build & Debug",
            type: "haltd",
            request: "launch",
            device: device.name,
            interface: "SWD",
            speed: 4000,
            elf: elfChoice.elfLaunchProp,
            flash: "changed",
            runToMain: true,
            ...(preLaunchTask ? { preLaunchTask } : {}),
          },
          {
            name: "Haltd: Attach",
            type: "haltd",
            request: "attach",
            device: device.name,
            interface: "SWD",
            speed: 4000,
            elf: elfChoice.elfLaunchProp,
          },
        ];
      }
    }
  }
  return [
    {
      name: "Haltd: Build & Debug",
      type: "haltd",
      request: "launch",
      device: "STM32H723VG",
      interface: "SWD",
      speed: 4000,
      elf: "${workspaceFolder}/build/firmware.elf",
      flash: "changed",
      runToMain: true,
    },
    {
      name: "Haltd: Attach",
      type: "haltd",
      request: "attach",
      device: "STM32H723VG",
      interface: "SWD",
      speed: 4000,
      elf: "${workspaceFolder}/build/firmware.elf",
    },
  ];
}
