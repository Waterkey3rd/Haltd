import { strict as assert } from "node:assert";
import * as fs from "node:fs";
import * as path from "node:path";
import test from "node:test";
import {
  detectDeviceHints,
  deviceMatchesHints,
  inferDefaultElfRelPath,
  isArmElfHeader,
  mergeHaltdLaunchJson,
  resolveOrGenerateBuildTask,
} from "../projectSetupCore";

test("detects 32-bit little-endian ARM ELF headers only", () => {
  const arm = new Uint8Array(20);
  arm[0] = 0x7f;
  arm[1] = 0x45;
  arm[2] = 0x4c;
  arm[3] = 0x46;
  arm[4] = 1; // ELFCLASS32
  arm[5] = 1; // ELFDATA2LSB
  arm[18] = 40; // EM_ARM
  assert.equal(isArmElfHeader(arm), true);

  const x64 = new Uint8Array(arm);
  x64[4] = 2;
  x64[18] = 62; // EM_X86_64
  assert.equal(isArmElfHeader(x64), false);
});

test("detects device hints from CubeMX .ioc and CMakeLists.txt", () => {
  const hints = detectDeviceHints([
    { name: "screen.ioc", content: "Mcu.UserName=STM32F103C8Tx\nProjectManager.ProjectName=screen\n" },
    { name: "CMakeLists.txt", content: "add_definitions(-DSTM32H723xx)\n" },
  ]);
  assert.equal(deviceMatchesHints("STM32F103C8", hints), "exact");
  assert.equal(deviceMatchesHints("STM32F103CB", hints), "prefix");
  assert.equal(deviceMatchesHints("STM32H723VG", hints), "prefix");
  assert.equal(deviceMatchesHints("STM32G474RE", hints), undefined);
});

test("infers ELF path and generates build task when tasks.json is absent", () => {
  const elfPath = inferDefaultElfRelPath({
    cmakeLists: "set(CMAKE_PROJECT_NAME eng27Arm)\nproject(${CMAKE_PROJECT_NAME} LANGUAGES C CXX)\n",
    cmakePresets: JSON.stringify({ configurePresets: [{ binaryDir: "${sourceDir}/build/Debug" }] }),
  });
  assert.equal(elfPath, "build/Debug/eng27Arm.elf");

  const taskDecision = resolveOrGenerateBuildTask(undefined, elfPath, {
    cmakeLists: "project(eng27Arm)",
  });
  assert.equal(taskDecision.defaultLabel, "CMake: build");
  assert.ok(taskDecision.generatedTask?.tasksJsonContent.includes('"isDefault": true'));
  assert.ok(taskDecision.generatedTask?.tasksJsonContent.includes('"$haltd-gcc"'));
});

test("merges Build & Debug (runToMain: true) and Attach while preserving JSONC comments", () => {
  const created = mergeHaltdLaunchJson(undefined, {
    device: "STM32H723VG",
    elf: "${workspaceFolder}/build/eng27Arm.elf",
    preLaunchTask: "CMake: build",
  });
  const createdParsed = JSON.parse(created) as {
    configurations: Array<Record<string, unknown>>;
  };
  assert.equal(createdParsed.configurations.length, 2);
  assert.equal(createdParsed.configurations[0].request, "launch");
  assert.equal(createdParsed.configurations[0].runToMain, true);
  assert.equal(createdParsed.configurations[0].preLaunchTask, "CMake: build");
  assert.equal(createdParsed.configurations[1].request, "attach");

  const existing = `{
    // Keep user comment
    "version": "0.2.0",
    "configurations": [
        {
            "name": "Other Debugger",
            "type": "cortex-debug",
            "request": "launch"
        }
    ]
}
`;
  const merged = mergeHaltdLaunchJson(existing, {
    device: "STM32G474RE",
    elf: "${workspaceFolder}/build/foc.elf",
    preLaunchTask: "build",
  });
  assert.ok(merged.includes("// Keep user comment"));
  assert.ok(merged.includes('"type": "cortex-debug"'));
  assert.ok(merged.includes('"request": "launch"'));
  assert.ok(merged.includes('"request": "attach"'));
  assert.ok(merged.includes('"runToMain": true'));
});

test("manifest registers onDebugInitialConfigurations and omits static initialConfigurations", () => {
  const pkgPath = [
    path.resolve(__dirname, "../package.json"),
    path.resolve(__dirname, "../../package.json"),
  ].find((candidate) => fs.existsSync(candidate));
  assert.ok(pkgPath, "expected vscode-ext/package.json to exist");
  const pkg = JSON.parse(fs.readFileSync(pkgPath, "utf8")) as {
    name?: string;
    activationEvents?: string[];
    contributes?: {
      debuggers?: Array<{
        initialConfigurations?: unknown;
        configurationSnippets?: unknown[];
      }>;
    };
  };
  assert.equal(pkg.name, "haltd-embedded");
  assert.ok(pkg.activationEvents?.includes("onDebugInitialConfigurations"));
  const dbg = pkg.contributes?.debuggers?.[0];
  assert.ok(dbg);
  assert.equal(dbg.initialConfigurations, undefined);
  assert.equal(dbg.configurationSnippets?.length, 2);
});
