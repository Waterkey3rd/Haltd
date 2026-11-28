import { strict as assert } from "node:assert";
import test from "node:test";
import {
  applyMirrorPrefix,
  bestSvdMatch,
  buildDirectRawCandidateUrls,
  cmsisSvdVendorForDevice,
  isValidSvdXml,
  matchSvdScore,
  normalizeDeviceName,
  stm32DfpRepoForDevice,
} from "../svdCacheCore";

test("normalizes J-Link device names and matches wildcards accurately", () => {
  assert.equal(normalizeDeviceName("STM32H745XI_M7"), "STM32H745XI");
  assert.equal(normalizeDeviceName("stm32g474re (M4)"), "STM32G474RE");

  const entries = [
    { name: "STM32F1xx.svd" },
    { name: "STM32F103xx.svd" },
    { name: "STM32H7xx.svd" },
    { name: "STM32H7x3.svd" },
    { name: "STM32H743.svd" },
    { name: "STM32L4x6.svd" },
    { name: "STM32H745_CM7.svd" },
    { name: "STM32H745_CM4.svd" },
    { name: "STM32G474xx.svd" },
  ];

  assert.equal(bestSvdMatch("STM32F103C8", entries)?.name, "STM32F103xx.svd");
  assert.equal(bestSvdMatch("STM32H723VG", entries)?.name, "STM32H7x3.svd");
  assert.equal(bestSvdMatch("STM32H743ZI", entries)?.name, "STM32H743.svd");
  assert.equal(bestSvdMatch("STM32L476RG", entries)?.name, "STM32L4x6.svd");
  assert.equal(bestSvdMatch("STM32H745XI_M7", entries)?.name, "STM32H745_CM7.svd");
  assert.equal(bestSvdMatch("STM32G474RE", entries)?.name, "STM32G474xx.svd");
  assert.equal(matchSvdScore("LPC1768", "STM32F1xx.svd"), undefined);
});

test("maps official DFP / cmsis-svd repositories and applies mirror prefix", () => {
  assert.equal(stm32DfpRepoForDevice("STM32H723VG"), "STM32H7xx_DFP");
  assert.equal(stm32DfpRepoForDevice("STM32H7R3L8"), "STM32H7RSxx_DFP");
  assert.equal(stm32DfpRepoForDevice("STM32G474RE"), "STM32G4xx_DFP");
  assert.equal(stm32DfpRepoForDevice("STM32WBA52CG"), "STM32WBAxx_DFP");
  assert.equal(cmsisSvdVendorForDevice("nRF52840_xxAA", "Nordic"), "Nordic");

  assert.equal(
    applyMirrorPrefix("https://raw.githubusercontent.com/a/b.svd", "https://mirror.example.com/"),
    "https://mirror.example.com/https://raw.githubusercontent.com/a/b.svd",
  );
  const candidates = buildDirectRawCandidateUrls("STM32H723VG");
  assert.ok(
    candidates.some(
      (c) =>
        c.filename === "STM32H723.svd" &&
        c.url === "https://raw.githubusercontent.com/Open-CMSIS-Pack/STM32H7xx_DFP/main/CMSIS/SVD/STM32H723.svd",
    ),
  );
});

test("validates downloaded SVD XML and rejects HTML error pages", () => {
  assert.equal(isValidSvdXml("<!DOCTYPE html><html>404</html>"), false);
  assert.equal(isValidSvdXml("<device><name>STM32H723</name><peripherals></peripherals></device>"), true);
});
