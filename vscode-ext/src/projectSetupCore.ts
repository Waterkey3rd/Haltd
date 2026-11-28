export interface DeviceHintMatch {
  exactUpper: string[];
  prefixesUpper: string[];
}

export function isArmElfHeader(header: Uint8Array): boolean {
  if (header.length < 20) {
    return false;
  }
  // ELF magic, ELFCLASS32 (1), little-endian (1), e_machine == EM_ARM (40)
  return (
    header[0] === 0x7f &&
    header[1] === 0x45 &&
    header[2] === 0x4c &&
    header[3] === 0x46 &&
    header[4] === 1 &&
    header[5] === 1 &&
    (header[18] | (header[19] << 8)) === 40
  );
}

function addMcuHint(raw: string, exactSet: Set<string>, prefixSet: Set<string>): void {
  const upper = raw.trim().toUpperCase();
  if (!/^STM32[A-Z0-9]{4,}/.test(upper)) {
    return;
  }
  // Strip CubeMX package suffixes like `STM32F103C8Tx` or `STM32H723VGTx`.
  const noCubePkg = upper.replace(/^((STM32[A-Z0-9]{6}))[A-Z][A-Z0-9]?X?$/, "$1");
  const corePrefix = noCubePkg.replace(/X+$/, "");
  if (corePrefix.includes("X")) {
    const beforeX = corePrefix.split("X")[0];
    if (beforeX.length >= 7) {
      prefixSet.add(beforeX);
    }
    return;
  }
  if (corePrefix.length >= 11) {
    exactSet.add(corePrefix);
    prefixSet.add(corePrefix.slice(0, 9));
  } else if (corePrefix.length >= 7) {
    prefixSet.add(corePrefix);
  }
}

export function detectDeviceHints(files: ReadonlyArray<{ name: string; content: string }>): DeviceHintMatch {
  const exactSet = new Set<string>();
  const prefixSet = new Set<string>();

  for (const { name, content } of files) {
    const lower = name.toLowerCase();
    if (lower.endsWith(".ioc")) {
      const mUser = content.match(/^Mcu\.UserName\s*=\s*(.+)$/m);
      if (mUser) addMcuHint(mUser[1], exactSet, prefixSet);
      const mId = content.match(/^ProjectManager\.DeviceId\s*=\s*(.+)$/m);
      if (mId) addMcuHint(mId[1], exactSet, prefixSet);
      const mName = content.match(/^Mcu\.Name\s*=\s*(.+)$/m);
      if (mName) addMcuHint(mName[1], exactSet, prefixSet);
    } else if (lower.endsWith(".jdebug") || lower.endsWith("launch.json")) {
      for (const m of content.matchAll(/"?(?:device|Device)"?\s*[:(]\s*"([^"]+)"/g)) {
        addMcuHint(m[1], exactSet, prefixSet);
      }
    }

    for (const m of content.matchAll(/(?:-D|\b)(STM32[A-Za-z0-9]{4,})\b/gi)) {
      addMcuHint(m[1], exactSet, prefixSet);
    }
  }

  return {
    exactUpper: [...exactSet],
    prefixesUpper: [...prefixSet],
  };
}

export function deviceMatchesHints(deviceName: string, hints: DeviceHintMatch): "exact" | "prefix" | undefined {
  const upper = deviceName.toUpperCase();
  if (hints.exactUpper.some((e) => upper.startsWith(e))) {
    return "exact";
  }
  if (hints.prefixesUpper.some((p) => upper.startsWith(p))) {
    return "prefix";
  }
  return undefined;
}

export interface ProjectBuildFiles {
  cmakeLists?: string;
  cmakePresets?: string;
  makefile?: string;
  ioc?: string;
  hasNinjaBuild?: boolean;
}

function expandCmakeVars(expr: string, vars: Map<string, string>): string {
  return expr.replace(/\$\{([^}]+)\}/g, (_, key: string) => vars.get(key) ?? "");
}

export function inferDefaultElfRelPath(files: ProjectBuildFiles): string {
  let targetName: string | undefined;
  let buildDir = "build";

  if (files.cmakePresets) {
    try {
      const parsed = JSON.parse(files.cmakePresets) as {
        configurePresets?: Array<{ name?: string; binaryDir?: string }>;
      };
      const firstDir = parsed.configurePresets?.find((p) => typeof p.binaryDir === "string")?.binaryDir;
      if (firstDir) {
        buildDir = firstDir
          .replace("${sourceDir}/", "")
          .replace("${sourceDir}", ".")
          .replace("${presetName}", "Debug")
          .replace(/\/+$/, "");
      }
    } catch {
      // Ignore malformed presets
    }
  }

  if (files.cmakeLists) {
    const vars = new Map<string, string>();
    for (const m of files.cmakeLists.matchAll(/set\s*\(\s*([A-Za-z0-9_]+)\s+([^\s)]+)/gi)) {
      vars.set(m[1], m[2].replace(/^["']|["']$/g, ""));
    }
    const projMatch = files.cmakeLists.match(/project\s*\(\s*([^\s)]+)/i);
    if (projMatch) {
      const resolved = expandCmakeVars(projMatch[1].replace(/^["']|["']$/g, ""), vars);
      if (resolved) targetName = resolved;
    }
    const exeMatch = files.cmakeLists.match(/add_executable\s*\(\s*([^\s)]+)/i);
    if (exeMatch) {
      const resolved = expandCmakeVars(exeMatch[1].replace(/^["']|["']$/g, ""), vars);
      if (resolved) targetName = resolved;
    }
  }

  if (!targetName && files.makefile) {
    const t = files.makefile.match(/^\s*TARGET\s*[:?]?=\s*([^\s#]+)/m);
    if (t) targetName = t[1];
    const b = files.makefile.match(/^\s*BUILD_DIR\s*[:?]?=\s*([^\s#]+)/m);
    if (b) buildDir = b[1].replace(/\/+$/, "");
  }

  if (!targetName && files.ioc) {
    const p = files.ioc.match(/^ProjectManager\.ProjectName\s*=\s*(.+)$/m);
    if (p) targetName = p[1].trim();
  }

  const file = `${targetName || "firmware"}.elf`;
  return buildDir && buildDir !== "." ? `${buildDir}/${file}` : file;
}

export interface BuildTaskDecision {
  existingLabels: string[];
  defaultLabel?: string;
  generatedTask?: {
    label: string;
    tasksJsonContent: string;
  };
}

interface ParsedTasksJson {
  version?: string;
  tasks?: Array<{
    label?: string;
    group?: string | { kind?: string; isDefault?: boolean };
  }>;
}

function stripJsonComments(source: string): string {
  let out = "";
  let inStr = false;
  for (let i = 0; i < source.length; i++) {
    const ch = source[i];
    if (inStr) {
      out += ch;
      if (ch === "\\") {
        if (i + 1 < source.length) out += source[++i];
      } else if (ch === '"') {
        inStr = false;
      }
      continue;
    }
    if (ch === '"') {
      inStr = true;
      out += ch;
    } else if (ch === "/" && source[i + 1] === "/") {
      while (i < source.length && source[i] !== "\n") i++;
      out += "\n";
    } else if (ch === "/" && source[i + 1] === "*") {
      i += 2;
      while (i + 1 < source.length && !(source[i] === "*" && source[i + 1] === "/")) i++;
      i++;
    } else {
      out += ch;
    }
  }
  return out.replace(/,(\s*[}\]])/g, "$1");
}

function inferBuildDirForTask(elfRelPath: string, files: ProjectBuildFiles): string {
  if (files.cmakePresets) {
    try {
      const parsed = JSON.parse(files.cmakePresets) as {
        configurePresets?: Array<{ binaryDir?: string }>;
      };
      const bd = parsed.configurePresets?.find((p) => typeof p.binaryDir === "string")?.binaryDir;
      if (bd) {
        return bd.replace("${sourceDir}", "${workspaceFolder}").replace("${presetName}", "Debug");
      }
    } catch {
      // Ignore
    }
  }
  const normalized = elfRelPath.replace(/\\/g, "/");
  const idx = normalized.lastIndexOf("/");
  if (idx > 0) {
    return `\${workspaceFolder}/${normalized.slice(0, idx)}`;
  }
  return "${workspaceFolder}/build";
}

export function resolveOrGenerateBuildTask(
  existingTasksJson: string | undefined,
  elfRelPath: string,
  files: ProjectBuildFiles,
): BuildTaskDecision {
  let parsed: ParsedTasksJson | undefined;
  if (existingTasksJson) {
    try {
      parsed = JSON.parse(stripJsonComments(existingTasksJson)) as ParsedTasksJson;
    } catch {
      parsed = undefined;
    }
  }

  const existingBuildTasks = (parsed?.tasks ?? []).filter((t) => {
    if (!t.label) return false;
    if (typeof t.group === "string") return t.group === "build";
    return t.group?.kind === "build";
  });

  if (existingBuildTasks.length > 0) {
    const labels = existingBuildTasks.map((t) => t.label!);
    const defaultTask = existingBuildTasks.find(
      (t) => typeof t.group === "object" && t.group?.isDefault === true,
    );
    return {
      existingLabels: labels,
      defaultLabel: defaultTask?.label ?? (labels.length === 1 ? labels[0] : undefined),
    };
  }

  let newTaskObj: Record<string, unknown> | undefined;
  if (files.cmakeLists || files.cmakePresets) {
    const buildDir = inferBuildDirForTask(elfRelPath, files);
    newTaskObj = {
      label: "CMake: build",
      type: "process",
      command: "cmake",
      args: ["--build", buildDir],
      group: { kind: "build", isDefault: true },
      problemMatcher: "$haltd-gcc",
    };
  } else if (files.makefile) {
    newTaskObj = {
      label: "build",
      type: "shell",
      command: "make -j",
      group: { kind: "build", isDefault: true },
      problemMatcher: "$haltd-gcc",
    };
  } else if (files.hasNinjaBuild) {
    const buildDir = inferBuildDirForTask(elfRelPath, files);
    newTaskObj = {
      label: "build",
      type: "process",
      command: "ninja",
      args: ["-C", buildDir],
      group: { kind: "build", isDefault: true },
      problemMatcher: "$haltd-gcc",
    };
  }

  if (!newTaskObj) {
    return { existingLabels: [] };
  }

  const label = String(newTaskObj.label);
  let tasksJsonContent: string;
  if (existingTasksJson && parsed && Array.isArray(parsed.tasks)) {
    const inserted = insertObjectIntoJsonArray(existingTasksJson, "tasks", newTaskObj);
    tasksJsonContent = inserted ?? JSON.stringify({ version: "2.0.0", tasks: [...parsed.tasks, newTaskObj] }, null, 4) + "\n";
  } else {
    tasksJsonContent = JSON.stringify({ version: "2.0.0", tasks: [newTaskObj] }, null, 4) + "\n";
  }
  return {
    existingLabels: [],
    defaultLabel: label,
    generatedTask: { label, tasksJsonContent },
  };
}

interface ArraySpan {
  openBracket: number;
  closeBracket: number;
  objects: Array<{ start: number; end: number; text: string }>;
}

function findArraySpan(source: string, keyName: string): ArraySpan | undefined {
  let inStr = false;
  let strStart = -1;
  let depthBrace = 0;

  for (let i = 0; i < source.length; i++) {
    const ch = source[i];
    if (inStr) {
      if (ch === "\\") {
        i++;
      } else if (ch === '"') {
        inStr = false;
        const keyText = source.slice(strStart + 1, i);
        if (depthBrace === 1 && keyText === keyName) {
          let j = i + 1;
          while (j < source.length && /\s/.test(source[j])) j++;
          if (source[j] === ":") {
            j++;
            while (j < source.length && /\s/.test(source[j])) j++;
            if (source[j] === "[") {
              return parseArrayObjects(source, j);
            }
          }
        }
      }
      continue;
    }
    if (ch === "/" && source[i + 1] === "/") {
      while (i < source.length && source[i] !== "\n") i++;
      continue;
    }
    if (ch === "/" && source[i + 1] === "*") {
      i += 2;
      while (i + 1 < source.length && !(source[i] === "*" && source[i + 1] === "/")) i++;
      i++;
      continue;
    }
    if (ch === '"') {
      inStr = true;
      strStart = i;
    } else if (ch === "{") {
      depthBrace++;
    } else if (ch === "}") {
      depthBrace--;
    }
  }
  return undefined;
}

function parseArrayObjects(source: string, openBracket: number): ArraySpan | undefined {
  let depthBracket = 0;
  let depthBrace = 0;
  let inStr = false;
  let objStart = -1;
  const objects: Array<{ start: number; end: number; text: string }> = [];

  for (let i = openBracket; i < source.length; i++) {
    const ch = source[i];
    if (inStr) {
      if (ch === "\\") i++;
      else if (ch === '"') inStr = false;
      continue;
    }
    if (ch === "/" && source[i + 1] === "/") {
      while (i < source.length && source[i] !== "\n") i++;
      continue;
    }
    if (ch === "/" && source[i + 1] === "*") {
      i += 2;
      while (i + 1 < source.length && !(source[i] === "*" && source[i + 1] === "/")) i++;
      i++;
      continue;
    }
    if (ch === '"') {
      inStr = true;
    } else if (ch === "[" && depthBrace === 0) {
      depthBracket++;
    } else if (ch === "]" && depthBrace === 0) {
      depthBracket--;
      if (depthBracket === 0) {
        return { openBracket, closeBracket: i, objects };
      }
    } else if (ch === "{") {
      if (depthBrace === 0 && depthBracket === 1) {
        objStart = i;
      }
      depthBrace++;
    } else if (ch === "}") {
      depthBrace--;
      if (depthBrace === 0 && depthBracket === 1 && objStart >= 0) {
        objects.push({ start: objStart, end: i + 1, text: source.slice(objStart, i + 1) });
        objStart = -1;
      }
    }
  }
  return undefined;
}

function indentObject(obj: Record<string, unknown>, baseIndent: string): string {
  const lines = JSON.stringify(obj, null, 4).split("\n");
  return lines.map((line, idx) => (idx === 0 ? line : `${baseIndent}${line}`)).join("\n");
}

function insertObjectIntoJsonArray(
  source: string,
  arrayKey: string,
  obj: Record<string, unknown>,
): string | undefined {
  const span = findArraySpan(source, arrayKey);
  if (!span) {
    return undefined;
  }
  const indent = "        ";
  const formatted = indentObject(obj, indent);
  if (span.objects.length === 0) {
    return `${source.slice(0, span.openBracket + 1)}\n${indent}${formatted}\n    ${source.slice(span.closeBracket)}`;
  }
  const last = span.objects[span.objects.length - 1];
  const between = source.slice(last.end, span.closeBracket);
  const hasTrailingComma = /,/.test(stripJsonComments(between));
  const prefix = hasTrailingComma ? "" : ",";
  return `${source.slice(0, last.end)}${prefix}\n${indent}${formatted}${source.slice(last.end)}`;
}

export interface HaltdLaunchParams {
  device: string;
  elf: string;
  preLaunchTask?: string;
}

/**
 * Create or non-destructively update `.vscode/launch.json` with both
 * `Haltd: Build & Debug` (`runToMain: true`) and `Haltd: Attach`.
 */
export function mergeHaltdLaunchJson(
  existingText: string | undefined,
  params: HaltdLaunchParams,
): string {
  const launchCfg: Record<string, unknown> = {
    name: "Haltd: Build & Debug",
    type: "haltd",
    request: "launch",
    device: params.device,
    interface: "SWD",
    speed: 4000,
    elf: params.elf,
    flash: "changed",
    runToMain: true,
    ...(params.preLaunchTask ? { preLaunchTask: params.preLaunchTask } : {}),
  };
  const attachCfg: Record<string, unknown> = {
    name: "Haltd: Attach",
    type: "haltd",
    request: "attach",
    device: params.device,
    interface: "SWD",
    speed: 4000,
    elf: params.elf,
  };

  if (!existingText || !existingText.trim()) {
    return (
      JSON.stringify(
        {
          version: "0.2.0",
          configurations: [launchCfg, attachCfg],
        },
        null,
        4,
      ) + "\n"
    );
  }

  const span = findArraySpan(existingText, "configurations");
  if (!span) {
    return (
      JSON.stringify(
        {
          version: "0.2.0",
          configurations: [launchCfg, attachCfg],
        },
        null,
        4,
      ) + "\n"
    );
  }

  let launchObjSpan: { start: number; end: number; existing: Record<string, unknown> } | undefined;
  let attachObjSpan: { start: number; end: number; existing: Record<string, unknown> } | undefined;

  for (const obj of span.objects) {
    try {
      const parsed = JSON.parse(stripJsonComments(obj.text)) as Record<string, unknown>;
      if (parsed.type !== "haltd") continue;
      const req = parsed.request ?? "launch";
      if (req === "launch" && !launchObjSpan) {
        launchObjSpan = { start: obj.start, end: obj.end, existing: parsed };
      } else if (req === "attach" && !attachObjSpan) {
        attachObjSpan = { start: obj.start, end: obj.end, existing: parsed };
      }
    } catch {
      // Skip malformed entry
    }
  }

  const replacements: Array<{ start: number; end: number; text: string }> = [];
  if (launchObjSpan) {
    const merged: Record<string, unknown> = {
      ...launchObjSpan.existing,
      type: "haltd",
      request: "launch",
      device: params.device,
      interface: launchObjSpan.existing.interface ?? "SWD",
      speed: launchObjSpan.existing.speed ?? 4000,
      elf: params.elf,
      flash: launchObjSpan.existing.flash ?? "changed",
      runToMain: true,
      ...(params.preLaunchTask ? { preLaunchTask: params.preLaunchTask } : {}),
    };
    const lineStart = existingText.lastIndexOf("\n", launchObjSpan.start) + 1;
    const indent = existingText.slice(lineStart, launchObjSpan.start).match(/^(\s*)/)?.[1] ?? "        ";
    replacements.push({
      start: launchObjSpan.start,
      end: launchObjSpan.end,
      text: indentObject(merged, indent),
    });
  }
  if (attachObjSpan) {
    const merged: Record<string, unknown> = {
      ...attachObjSpan.existing,
      type: "haltd",
      request: "attach",
      device: params.device,
      interface: attachObjSpan.existing.interface ?? "SWD",
      speed: attachObjSpan.existing.speed ?? 4000,
      elf: params.elf,
    };
    const lineStart = existingText.lastIndexOf("\n", attachObjSpan.start) + 1;
    const indent = existingText.slice(lineStart, attachObjSpan.start).match(/^(\s*)/)?.[1] ?? "        ";
    replacements.push({
      start: attachObjSpan.start,
      end: attachObjSpan.end,
      text: indentObject(merged, indent),
    });
  }

  let updated = existingText;
  replacements.sort((a, b) => b.start - a.start);
  for (const rep of replacements) {
    updated = `${updated.slice(0, rep.start)}${rep.text}${updated.slice(rep.end)}`;
  }

  if (!launchObjSpan) {
    updated = insertObjectIntoJsonArray(updated, "configurations", launchCfg) ?? updated;
  }
  if (!attachObjSpan) {
    updated = insertObjectIntoJsonArray(updated, "configurations", attachCfg) ?? updated;
  }
  return updated;
}
