import * as fs from "fs";
import * as os from "os";
import * as path from "path";
import * as vscode from "vscode";
import {
  applyMirrorPrefix,
  bestSvdMatch,
  buildDirectRawCandidateUrls,
  cmsisSvdVendorForDevice,
  isValidSvdXml,
  normalizeDeviceName,
  stm32DfpRepoForDevice,
} from "./svdCacheCore";

const INDEX_TTL_MS = 30 * 24 * 60 * 60 * 1000;
const FETCH_TIMEOUT_MS = 10_000;

interface RemoteSvdEntry {
  name: string;
  downloadUrl: string;
}

interface CachedRepoIndex {
  fetchedAt: number;
  entries: RemoteSvdEntry[];
}

/** Mirror `crates/symbols/src/paths.rs::cache_root()`. */
export function haltdCacheRoot(): string {
  const envDir = process.env.HALTD_CACHE_DIR?.trim();
  if (envDir) {
    return envDir;
  }
  if (process.platform === "win32") {
    return path.join(os.homedir(), "Documents", "haltd");
  }
  const xdg = process.env.XDG_CACHE_HOME?.trim();
  if (xdg) {
    return path.join(xdg, "haltd");
  }
  return path.join(os.homedir(), ".cache", "haltd");
}

function listSvdFiles(dir: string): Array<{ name: string; fullPath: string }> {
  try {
    return fs
      .readdirSync(dir, { withFileTypes: true })
      .filter((entry) => entry.isFile() && entry.name.toLowerCase().endsWith(".svd"))
      .map((entry) => ({ name: entry.name, fullPath: path.join(dir, entry.name) }));
  } catch {
    return [];
  }
}

function ozoneSvdDirs(): string[] {
  const base = process.platform === "win32" ? "C:\\Program Files\\SEGGER" : "/opt/SEGGER";
  try {
    return fs
      .readdirSync(base, { withFileTypes: true })
      .filter((entry) => entry.isDirectory() && entry.name.startsWith("Ozone"))
      .map((entry) => path.join(base, entry.name, "Config", "Peripherals"));
  } catch {
    return [];
  }
}

function stToolchainSvdDirs(): string[] {
  const roots =
    process.platform === "win32"
      ? ["C:\\ST", "C:\\Program Files\\STMicroelectronics"]
      : ["/opt/st", "/opt/STMicroelectronics"];
  const dirs: string[] = [];
  for (const root of roots) {
    try {
      for (const entry of fs.readdirSync(root, { withFileTypes: true })) {
        if (!entry.isDirectory()) continue;
        for (const sub of ["STMicroelectronics_CMSIS_SVD", "SVD", "CMSIS/SVD"]) {
          const candidate = path.join(root, entry.name, sub);
          if (fs.existsSync(candidate)) {
            dirs.push(candidate);
          }
        }
      }
    } catch {
      // Directory absent
    }
  }
  return dirs;
}

async function fetchJsonWithTimeout<T>(url: string, timeoutMs: number): Promise<T | undefined> {
  try {
    const res = await fetch(url, {
      headers: {
        Accept: "application/vnd.github+json",
        "User-Agent": "haltd-vscode-ext",
      },
      signal: AbortSignal.timeout(timeoutMs),
    });
    if (!res.ok) {
      return undefined;
    }
    return (await res.json()) as T;
  } catch {
    return undefined;
  }
}

async function fetchTextWithTimeout(url: string, timeoutMs: number): Promise<string | undefined> {
  try {
    const res = await fetch(url, {
      headers: { "User-Agent": "haltd-vscode-ext" },
      signal: AbortSignal.timeout(timeoutMs),
    });
    if (!res.ok) {
      return undefined;
    }
    return await res.text();
  } catch {
    return undefined;
  }
}

export class SvdCacheManager {
  private readonly inFlight = new Map<string, Promise<string | undefined>>();

  constructor(private readonly log: (message: string) => void = () => undefined) {}

  private filesDir(): string {
    return path.join(haltdCacheRoot(), "svd", "files");
  }

  private indexDir(): string {
    return path.join(haltdCacheRoot(), "svd", "index");
  }

  private mirrorPrefix(): string {
    return vscode.workspace.getConfiguration("haltd").get<string>("svd.mirrorPrefix", "");
  }

  autoDownloadEnabled(): boolean {
    return vscode.workspace.getConfiguration("haltd").get<boolean>("svd.autoDownload", true);
  }

  findLocal(device: string): string | undefined {
    const searchDirs = [
      this.filesDir(),
      path.join(haltdCacheRoot(), "svd"),
      ...ozoneSvdDirs(),
      ...stToolchainSvdDirs(),
    ];
    for (const dir of searchDirs) {
      const hit = bestSvdMatch(device, listSvdFiles(dir));
      if (hit) {
        return hit.fullPath;
      }
    }
    return undefined;
  }

  async ensureForDevice(
    device: string,
    options?: { forceRefresh?: boolean; manufacturer?: string },
  ): Promise<string | undefined> {
    if (!options?.forceRefresh) {
      const local = this.findLocal(device);
      if (local) {
        return local;
      }
    }
    const key = `${normalizeDeviceName(device)}:${options?.forceRefresh ? "refresh" : "cached"}`;
    const existing = this.inFlight.get(key);
    if (existing) {
      return existing;
    }
    const task = this.fetchAndCache(device, options).finally(() => {
      this.inFlight.delete(key);
    });
    this.inFlight.set(key, task);
    return task;
  }

  private async loadRepoIndex(
    cacheKey: string,
    apiUrl: string,
    rawBaseUrl: string,
    forceRefresh: boolean,
  ): Promise<RemoteSvdEntry[] | undefined> {
    const indexPath = path.join(this.indexDir(), `${cacheKey}.json`);
    if (!forceRefresh) {
      try {
        const parsed = JSON.parse(fs.readFileSync(indexPath, "utf8")) as CachedRepoIndex;
        if (
          Array.isArray(parsed.entries) &&
          parsed.entries.length > 0 &&
          Date.now() - parsed.fetchedAt < INDEX_TTL_MS
        ) {
          return parsed.entries;
        }
      } catch {
        // Cache miss or stale
      }
    }

    const items = await fetchJsonWithTimeout<Array<{ name?: string; download_url?: string }>>(
      apiUrl,
      FETCH_TIMEOUT_MS,
    );
    if (!Array.isArray(items)) {
      return undefined;
    }
    const entries: RemoteSvdEntry[] = items
      .filter((item) => typeof item.name === "string" && item.name.toLowerCase().endsWith(".svd"))
      .map((item) => ({
        name: item.name!,
        downloadUrl: item.download_url || `${rawBaseUrl}/${item.name!}`,
      }));

    if (entries.length > 0) {
      try {
        fs.mkdirSync(this.indexDir(), { recursive: true });
        fs.writeFileSync(
          indexPath,
          JSON.stringify({ fetchedAt: Date.now(), entries } satisfies CachedRepoIndex, null, 2),
        );
      } catch {
        // Ignore index write failure
      }
    }
    return entries;
  }

  private async downloadAndStore(filename: string, rawUrl: string): Promise<string | undefined> {
    const url = applyMirrorPrefix(rawUrl, this.mirrorPrefix());
    const xml = await fetchTextWithTimeout(url, FETCH_TIMEOUT_MS);
    if (!xml || !isValidSvdXml(xml)) {
      return undefined;
    }
    const dir = this.filesDir();
    fs.mkdirSync(dir, { recursive: true });
    const tmpPath = path.join(dir, `${filename}.part.${process.pid}.${Date.now()}`);
    const destPath = path.join(dir, filename);
    try {
      fs.writeFileSync(tmpPath, xml, "utf8");
      fs.renameSync(tmpPath, destPath);
      this.log(`Cached SVD for ${filename} -> ${destPath}`);
      return destPath;
    } catch {
      try {
        fs.unlinkSync(tmpPath);
      } catch {
        // Ignore cleanup failure
      }
      return undefined;
    }
  }

  private async fetchAndCache(
    device: string,
    options?: { forceRefresh?: boolean; manufacturer?: string },
  ): Promise<string | undefined> {
    const force = Boolean(options?.forceRefresh);

    // 1. Official ST Open-CMSIS-Pack DFP repository
    const dfp = stm32DfpRepoForDevice(device);
    if (dfp) {
      const entries = await this.loadRepoIndex(
        `Open-CMSIS-Pack__${dfp}`,
        `https://api.github.com/repos/Open-CMSIS-Pack/${dfp}/contents/CMSIS/SVD`,
        `https://raw.githubusercontent.com/Open-CMSIS-Pack/${dfp}/main/CMSIS/SVD`,
        force,
      );
      const hit = entries ? bestSvdMatch(device, entries) : undefined;
      if (hit) {
        const saved = await this.downloadAndStore(hit.name, hit.downloadUrl);
        if (saved) {
          return saved;
        }
      }
    }

    // 2. Official multi-vendor cmsis-svd-data repository
    const vendor = cmsisSvdVendorForDevice(device, options?.manufacturer);
    if (vendor) {
      const entries = await this.loadRepoIndex(
        `cmsis-svd__${vendor}`,
        `https://api.github.com/repos/cmsis-svd/cmsis-svd-data/contents/data/${vendor}`,
        `https://raw.githubusercontent.com/cmsis-svd/cmsis-svd-data/main/data/${vendor}`,
        force,
      );
      const hit = entries ? bestSvdMatch(device, entries) : undefined;
      if (hit) {
        const saved = await this.downloadAndStore(hit.name, hit.downloadUrl);
        if (saved) {
          return saved;
        }
      }
    }

    // 3. Fallback when GitHub REST API is rate-limited: direct raw URLs
    const directCandidates = buildDirectRawCandidateUrls(
      device,
      options?.manufacturer,
      this.mirrorPrefix(),
    );
    for (const candidate of directCandidates) {
      const xml = await fetchTextWithTimeout(candidate.url, FETCH_TIMEOUT_MS);
      if (!xml || !isValidSvdXml(xml)) {
        continue;
      }
      const dir = this.filesDir();
      fs.mkdirSync(dir, { recursive: true });
      const tmpPath = path.join(dir, `${candidate.filename}.part.${process.pid}`);
      const destPath = path.join(dir, candidate.filename);
      try {
        fs.writeFileSync(tmpPath, xml, "utf8");
        fs.renameSync(tmpPath, destPath);
        this.log(`Cached SVD (direct raw) -> ${destPath}`);
        return destPath;
      } catch {
        try {
          fs.unlinkSync(tmpPath);
        } catch {
          // Ignore
        }
      }
    }

    this.log(`No official SVD file could be fetched for ${device}`);
    return undefined;
  }
}
