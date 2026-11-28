/** Strip J-Link core/package annotations such as `_M7`, ` (M4)`, etc. */
export function normalizeDeviceName(device: string): string {
  const trimmed = device.trim();
  const base = trimmed.split(/[ (/ _]/)[0] ?? trimmed;
  return base.toUpperCase();
}

function coreTag(text: string): "M7" | "M4" | "M33" | "M0P" | undefined {
  const upper = text.toUpperCase();
  if (upper.endsWith("_CM7") || upper.endsWith("_M7")) return "M7";
  if (upper.endsWith("_CM4") || upper.endsWith("_M4")) return "M4";
  if (upper.endsWith("_CM33") || upper.endsWith("_M33")) return "M33";
  if (upper.endsWith("_CM0P") || upper.endsWith("_M0P") || upper.endsWith("_CM0+")) return "M0P";
  return undefined;
}

function stripCoreSuffix(upper: string): string {
  for (const suffix of ["_CM33", "_M33", "_CM7", "_M7", "_CM4", "_M4", "_CM0P", "_M0P", "_CM0+"]) {
    if (upper.endsWith(suffix)) {
      return upper.slice(0, -suffix.length);
    }
  }
  return upper;
}

/** Score how well an SVD filename or stem matches `device` (higher is better). */
export function matchSvdScore(device: string, filenameOrStem: string): number | undefined {
  const dev = normalizeDeviceName(device);
  const devCore = coreTag(device);
  if (!dev) {
    return undefined;
  }
  const stem = filenameOrStem.replace(/\.svd$/i, "").toUpperCase();
  const stemCore = coreTag(stem);
  if (devCore && stemCore && devCore !== stemCore) {
    return undefined;
  }
  const stemBase = stripCoreSuffix(stem).replace(/_+$/, "");
  const core = stemBase.replace(/X+$/, "");
  if (!core || core.length > dev.length) {
    return undefined;
  }
  let score = 0;
  for (let i = 0; i < core.length; i++) {
    if (core[i] === dev[i]) {
      score += 10;
    } else if (core[i] === "X") {
      score += 3;
    } else {
      return undefined;
    }
  }
  const coreBonus = devCore && stemCore && devCore === stemCore ? 20 : 0;
  const trailingX = stemBase.length - core.length;
  const exactnessTie = Math.max(0, 9 - trailingX);
  return score * 10 + coreBonus + exactnessTie;
}

export function bestSvdMatch<T extends { name: string }>(device: string, items: readonly T[]): T | undefined {
  let best: { score: number; item: T } | undefined;
  for (const item of items) {
    const score = matchSvdScore(device, item.name);
    if (score !== undefined && (!best || score > best.score)) {
      best = { score, item };
    }
  }
  return best?.item;
}

/** Official `Open-CMSIS-Pack` repository name for an STM32 device. */
export function stm32DfpRepoForDevice(device: string): string | undefined {
  const dev = normalizeDeviceName(device);
  if (!dev.startsWith("STM32") || dev.length < 7) {
    return undefined;
  }
  if (dev.startsWith("STM32H7R") || dev.startsWith("STM32H7S")) {
    return "STM32H7RSxx_DFP";
  }
  if (dev.startsWith("STM32WBA")) return "STM32WBAxx_DFP";
  if (dev.startsWith("STM32WB")) return "STM32WBxx_DFP";
  if (dev.startsWith("STM32WL")) return "STM32WLxx_DFP";

  const family = dev.slice(5, 7);
  switch (family) {
    case "C0":
    case "F0":
    case "F1":
    case "F2":
    case "F3":
    case "F4":
    case "F7":
    case "G0":
    case "G4":
    case "H5":
    case "H7":
    case "L0":
    case "L1":
    case "L4":
    case "L5":
    case "N6":
    case "U0":
    case "U3":
    case "U5":
      return `STM32${family}xx_DFP`;
    default:
      return undefined;
  }
}

export function cmsisSvdVendorForDevice(device: string, manufacturer?: string): string | undefined {
  const dev = normalizeDeviceName(device);
  const mfg = (manufacturer ?? "").toUpperCase();
  if (dev.startsWith("STM32") || mfg.includes("ST")) return "STMicro";
  if (dev.startsWith("NRF") || mfg.includes("NORDIC")) return "Nordic";
  if (dev.startsWith("LPC") || dev.startsWith("MIMX") || dev.startsWith("MK") || mfg.includes("NXP")) {
    return "NXP";
  }
  if (dev.startsWith("GD32") || mfg.includes("GIGA")) return "GigaDevice";
  if (dev.startsWith("RP2") || mfg.includes("RASPBERRY")) return "RaspberryPi";
  if (dev.startsWith("ATSAM") || dev.startsWith("SAM") || mfg.includes("MICROCHIP") || mfg.includes("ATMEL")) {
    return "Microchip";
  }
  if (dev.startsWith("TM4C") || dev.startsWith("MSP432") || mfg.includes("TEXAS")) {
    return "TexasInstruments";
  }
  return undefined;
}

export function applyMirrorPrefix(rawUrl: string, mirrorPrefix?: string): string {
  const prefix = (mirrorPrefix ?? "").trim().replace(/\/+$/, "");
  if (!prefix) {
    return rawUrl;
  }
  return `${prefix}/${rawUrl}`;
}

export function buildDirectRawCandidateUrls(
  device: string,
  manufacturer?: string,
  mirrorPrefix?: string,
): Array<{ filename: string; url: string }> {
  const dev = normalizeDeviceName(device);
  const stems: string[] = [];
  if (dev.startsWith("STM32")) {
    if (dev.length >= 9) {
      const p9 = dev.slice(0, 9);
      stems.push(p9, `${p9}xx`, `${p9.slice(0, 7)}x${p9[8]}`);
    }
    if (dev.length >= 7) {
      stems.push(`${dev.slice(0, 7)}xx`);
    }
  } else if (dev.length >= 4) {
    stems.push(dev);
    const m = dev.match(/^([A-Z]+\d+)/);
    if (m && m[1] !== dev) {
      stems.push(m[1], `${m[1]}xx`);
    }
  }

  const out: Array<{ filename: string; url: string }> = [];
  const dfp = stm32DfpRepoForDevice(device);
  if (dfp) {
    for (const stem of stems) {
      const filename = `${stem}.svd`;
      out.push({
        filename,
        url: applyMirrorPrefix(
          `https://raw.githubusercontent.com/Open-CMSIS-Pack/${dfp}/main/CMSIS/SVD/${filename}`,
          mirrorPrefix,
        ),
      });
    }
  }
  const vendor = cmsisSvdVendorForDevice(device, manufacturer);
  if (vendor) {
    for (const stem of stems) {
      const filename = `${stem}.svd`;
      out.push({
        filename,
        url: applyMirrorPrefix(
          `https://raw.githubusercontent.com/cmsis-svd/cmsis-svd-data/main/data/${vendor}/${filename}`,
          mirrorPrefix,
        ),
      });
    }
  }
  return out;
}

export function isValidSvdXml(xml: string): boolean {
  const head = xml.slice(0, 1024).trimStart().toLowerCase();
  if (!head || head.startsWith("<!doctype html") || head.startsWith("<html")) {
    return false;
  }
  return xml.includes("<device") && xml.includes("<peripherals>") && xml.includes("<name>");
}
