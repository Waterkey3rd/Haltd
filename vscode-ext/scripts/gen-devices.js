#!/usr/bin/env node
"use strict";

const { execFileSync } = require("child_process");
const fs = require("fs");
const path = require("path");

const extRoot = path.resolve(__dirname, "..");
const repoRoot = path.resolve(extRoot, "..");
const outFile = path.join(extRoot, "resources", "devices-stm32.json");

function findHaltd() {
  const idx = process.argv.indexOf("--haltd");
  if (idx !== -1 && process.argv[idx + 1]) {
    return process.argv[idx + 1];
  }
  const exe = process.platform === "win32" ? "haltd.exe" : "haltd";
  const candidates = [
    path.join(repoRoot, "target", "release", exe),
    path.join(repoRoot, "target", "debug", exe),
    path.join(extRoot, "bin", exe),
  ].filter((candidate) => fs.existsSync(candidate));
  candidates.sort((a, b) => fs.statSync(b).mtimeMs - fs.statSync(a).mtimeMs);
  return candidates[0] ?? exe;
}

const haltd = findHaltd();
const raw = execFileSync(haltd, ["devices", "--json", "--filter", "STM32"], {
  encoding: "utf8",
  maxBuffer: 16 * 1024 * 1024,
});
const parsed = JSON.parse(raw);
fs.mkdirSync(path.dirname(outFile), { recursive: true });
fs.writeFileSync(outFile, JSON.stringify(parsed) + "\n", "utf8");
console.log(`Wrote ${parsed.length} STM32 devices to ${outFile}`);
