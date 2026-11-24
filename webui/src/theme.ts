/**
 * Theme manager for jlinkd WebUI (Timeline & RTT).
 *
 * Automatically follows VS Code's active color theme (Dark, Light, High Contrast
 * Dark, High Contrast Light) and inherits VS Code CSS custom properties when
 * embedded in the VS Code Webview, or falls back to OS `prefers-color-scheme`
 * when opened in an external browser. Users can also manually override the theme.
 */

export type ThemeKind = "dark" | "light" | "hc-dark" | "hc-light";
export type ThemePreference = "auto" | ThemeKind;

export interface CanvasTheme {
  kind: ThemeKind;
  isLight: boolean;
  isHighContrast: boolean;
  bg: string;
  plotBg: string;
  axisStroke: string;
  gridMajor: string;
  gridMinor: string;
  tickStroke: string;
  crosshair: string;
  sampleCursor: string;
  cursorA: string;
  cursorB: string;
  cursorSpanFill: string;
  hoverBoxBg: string;
  hoverBoxBorder: string;
  hoverBoxText: string;
  haltFill: string;
  haltStroke: string;
  faultColor: string;
  resetColor: string;
  reloadColor: string;
  samplingColor: string;
  selectionFill: string;
  selectionStroke: string;
  badgeText: string;
  palette: string[];
}

const STORAGE_KEY = "jlinkd.timeline.themePref";

const DARK_PALETTE = [
  "#4dabf7", // Blue
  "#ff922b", // Orange
  "#51cf66", // Green
  "#f06595", // Pink
  "#ffd43b", // Yellow
  "#b197fc", // Purple
  "#20c997", // Teal
  "#ff6b6b", // Red
  "#3bc9db", // Cyan
  "#94d82d", // Lime
];

const LIGHT_PALETTE = [
  "#1971c2", // Deep Blue
  "#d9480f", // Deep Orange
  "#2b8a3e", // Forest Green
  "#c2255c", // Crimson Pink
  "#e67700", // Amber
  "#6741d9", // Royal Purple
  "#099268", // Deep Teal
  "#c92a2a", // Deep Red
  "#0c8599", // Deep Cyan
  "#5c940d", // Olive Lime
];

const HC_DARK_PALETTE = [
  "#38bdf8",
  "#fb923c",
  "#4ade80",
  "#f472b6",
  "#facc15",
  "#c084fc",
  "#2dd4bf",
  "#f87171",
  "#22d3ee",
  "#a3e635",
];

const HC_LIGHT_PALETTE = [
  "#004a99",
  "#b33600",
  "#0f6620",
  "#990f4b",
  "#8c5000",
  "#4a1fb8",
  "#00664d",
  "#a80000",
  "#005f73",
  "#3b6e00",
];

const VSCODE_VAR_MAP: Record<string, string> = {
  "--vscode-editor-background": "--vs-editor-bg",
  "--vscode-editor-foreground": "--vs-editor-fg",
  "--vscode-sideBar-background": "--vs-sidebar-bg",
  "--vscode-sideBar-foreground": "--vs-sidebar-fg",
  "--vscode-sideBarSectionHeader-background": "--vs-header-bg",
  "--vscode-panel-border": "--vs-border",
  "--vscode-widget-border": "--vs-widget-border",
  "--vscode-input-background": "--vs-input-bg",
  "--vscode-input-foreground": "--vs-input-fg",
  "--vscode-input-border": "--vs-input-border",
  "--vscode-button-background": "--vs-btn-bg",
  "--vscode-button-foreground": "--vs-btn-fg",
  "--vscode-button-hoverBackground": "--vs-btn-hover-bg",
  "--vscode-focusBorder": "--vs-focus-border",
  "--vscode-descriptionForeground": "--vs-muted",
  "--vscode-errorForeground": "--vs-danger",
  "--vscode-editorWarning-foreground": "--vs-warning",
  "--vscode-list-hoverBackground": "--vs-hover-bg",
  "--vscode-badge-background": "--vs-badge-bg",
  "--vscode-badge-foreground": "--vs-badge-fg",
  "--vscode-font-family": "--vs-font-sans",
  "--vscode-editor-font-family": "--vs-font-mono",
};

function parseThemeKind(raw: string | null | undefined): ThemeKind | null {
  if (!raw) return null;
  const v = raw.toLowerCase().trim();
  if (v === "hc-light" || v.includes("high-contrast-light")) return "hc-light";
  if (v === "hc-dark" || v === "hc" || v.includes("high-contrast")) return "hc-dark";
  if (v === "light" || v.includes("vscode-light")) return "light";
  if (v === "dark" || v.includes("vscode-dark")) return "dark";
  return null;
}

function detectDomVscodeKind(): ThemeKind | null {
  const body = document.body;
  if (!body) return null;
  const dataKind = body.dataset.vscodeThemeKind;
  if (dataKind) {
    const parsed = parseThemeKind(dataKind);
    if (parsed) return parsed;
  }
  if (body.classList.contains("vscode-high-contrast-light")) return "hc-light";
  if (body.classList.contains("vscode-high-contrast")) return "hc-dark";
  if (body.classList.contains("vscode-light")) return "light";
  if (body.classList.contains("vscode-dark")) return "dark";
  return null;
}

function detectSystemKind(): ThemeKind {
  if (window.matchMedia?.("(prefers-contrast: more)").matches) {
    return window.matchMedia?.("(prefers-color-scheme: light)").matches ? "hc-light" : "hc-dark";
  }
  return window.matchMedia?.("(prefers-color-scheme: light)").matches ? "light" : "dark";
}

export class ThemeManager {
  private preference: ThemePreference = "auto";
  private vscodeKind: ThemeKind | null = null;
  private vscodeThemeName: string | null = null;
  private vscodeVars: Record<string, string> = {};
  private activeKind: ThemeKind = "dark";
  private readonly listeners = new Set<(theme: CanvasTheme) => void>();

  constructor() {
    // 1. Check initial URL query parameter (?theme=...) passed by VS Code BackendPanel
    const params = new URLSearchParams(window.location.search);
    const urlTheme = parseThemeKind(params.get("theme") ?? params.get("vscodeTheme"));
    if (urlTheme) {
      this.vscodeKind = urlTheme;
    }

    // 2. Check saved manual preference
    try {
      const saved = localStorage.getItem(STORAGE_KEY) as ThemePreference | null;
      if (saved === "auto" || saved === "dark" || saved === "light" || saved === "hc-dark" || saved === "hc-light") {
        this.preference = saved;
      }
    } catch {
      // Ignore storage access errors in restricted webviews.
    }

    // 3. Check if body already has VS Code theme classes
    const domKind = detectDomVscodeKind();
    if (domKind) {
      this.vscodeKind = domKind;
    }

    this.apply();
    this.setupListeners();
  }

  getPreference(): ThemePreference {
    return this.preference;
  }

  getActiveKind(): ThemeKind {
    return this.activeKind;
  }

  getVscodeThemeName(): string | null {
    return this.vscodeThemeName;
  }

  setPreference(pref: ThemePreference): void {
    this.preference = pref;
    try {
      localStorage.setItem(STORAGE_KEY, pref);
    } catch {
      // Ignore storage errors
    }
    this.apply();
  }

  onChange(listener: (theme: CanvasTheme) => void): () => void {
    this.listeners.add(listener);
    return () => this.listeners.delete(listener);
  }

  getCanvasTheme(): CanvasTheme {
    const kind = this.activeKind;
    const isLight = kind === "light" || kind === "hc-light";
    const isHighContrast = kind === "hc-dark" || kind === "hc-light";
    const style = getComputedStyle(document.documentElement);
    const css = (name: string, fallback: string) => style.getPropertyValue(name).trim() || fallback;

    if (kind === "hc-light") {
      return {
        kind,
        isLight: true,
        isHighContrast: true,
        bg: css("--bg", "#ffffff"),
        plotBg: css("--plot-bg", "#ffffff"),
        axisStroke: "#000000",
        gridMajor: "rgba(0, 0, 0, 0.35)",
        gridMinor: "rgba(0, 0, 0, 0.18)",
        tickStroke: "rgba(0, 0, 0, 0.55)",
        crosshair: "rgba(0, 0, 0, 0.75)",
        sampleCursor: "#9a5b00",
        cursorA: "#004a99",
        cursorB: "#a80000",
        cursorSpanFill: "rgba(0, 74, 153, 0.12)",
        hoverBoxBg: "rgba(255, 255, 255, 0.96)",
        hoverBoxBorder: "#000000",
        hoverBoxText: "#000000",
        haltFill: "rgba(180, 0, 0, 0.14)",
        haltStroke: "rgba(180, 0, 0, 0.65)",
        faultColor: "#b30000",
        resetColor: "#995400",
        reloadColor: "#005a9e",
        samplingColor: "#0f6620",
        selectionFill: "rgba(0, 74, 153, 0.18)",
        selectionStroke: "#004a99",
        badgeText: "#ffffff",
        palette: HC_LIGHT_PALETTE,
      };
    }

    if (kind === "hc-dark") {
      return {
        kind,
        isLight: false,
        isHighContrast: true,
        bg: css("--bg", "#000000"),
        plotBg: css("--plot-bg", "#000000"),
        axisStroke: "#ffffff",
        gridMajor: "rgba(255, 255, 255, 0.35)",
        gridMinor: "rgba(255, 255, 255, 0.18)",
        tickStroke: "rgba(255, 255, 255, 0.55)",
        crosshair: "rgba(255, 255, 255, 0.85)",
        sampleCursor: "#facc15",
        cursorA: "#38bdf8",
        cursorB: "#fb7185",
        cursorSpanFill: "rgba(56, 189, 248, 0.14)",
        hoverBoxBg: "rgba(0, 0, 0, 0.95)",
        hoverBoxBorder: "#ffffff",
        hoverBoxText: "#ffffff",
        haltFill: "rgba(255, 80, 80, 0.18)",
        haltStroke: "rgba(255, 100, 100, 0.75)",
        faultColor: "#ff5555",
        resetColor: "#facc15",
        reloadColor: "#38bdf8",
        samplingColor: "#4ade80",
        selectionFill: "rgba(56, 189, 248, 0.22)",
        selectionStroke: "#38bdf8",
        badgeText: "#000000",
        palette: HC_DARK_PALETTE,
      };
    }

    if (isLight) {
      return {
        kind,
        isLight: true,
        isHighContrast: false,
        bg: css("--bg", "#f8f9fa"),
        plotBg: css("--plot-bg", "#ffffff"),
        axisStroke: css("--muted", "#57606a"),
        gridMajor: "rgba(31, 35, 40, 0.14)",
        gridMinor: "rgba(31, 35, 40, 0.06)",
        tickStroke: "rgba(31, 35, 40, 0.25)",
        crosshair: "rgba(25, 113, 194, 0.65)",
        sampleCursor: "#d97706",
        cursorA: "#0969da",
        cursorB: "#cf222e",
        cursorSpanFill: "rgba(9, 105, 218, 0.08)",
        hoverBoxBg: "rgba(255, 255, 255, 0.94)",
        hoverBoxBorder: "rgba(31, 35, 40, 0.28)",
        hoverBoxText: "#1f2328",
        haltFill: "rgba(207, 34, 46, 0.09)",
        haltStroke: "rgba(207, 34, 46, 0.4)",
        faultColor: "#cf222e",
        resetColor: "#bc4c00",
        reloadColor: "#0969da",
        samplingColor: "#1a7f37",
        selectionFill: "rgba(9, 105, 218, 0.14)",
        selectionStroke: "#0969da",
        badgeText: "#ffffff",
        palette: LIGHT_PALETTE,
      };
    }

    return {
      kind: "dark",
      isLight: false,
      isHighContrast,
      bg: css("--bg", "#1e1f22"),
      plotBg: css("--plot-bg", "#18191c"),
      axisStroke: css("--muted", "#9aa0a6"),
      gridMajor: "rgba(255, 255, 255, 0.11)",
      gridMinor: "rgba(255, 255, 255, 0.05)",
      tickStroke: "rgba(255, 255, 255, 0.18)",
      crosshair: "rgba(77, 171, 247, 0.65)",
      sampleCursor: "#fcc419",
      cursorA: "#4dabf7",
      cursorB: "#ff8787",
      cursorSpanFill: "rgba(77, 171, 247, 0.08)",
      hoverBoxBg: "rgba(24, 26, 31, 0.94)",
      hoverBoxBorder: "rgba(255, 255, 255, 0.22)",
      hoverBoxText: "#e6edf3",
      haltFill: "rgba(255, 100, 100, 0.12)",
      haltStroke: "rgba(255, 107, 107, 0.45)",
      faultColor: "#ff6b6b",
      resetColor: "#ffd43b",
      reloadColor: "#4dabf7",
      samplingColor: "#51cf66",
      selectionFill: "rgba(77, 171, 247, 0.16)",
      selectionStroke: "#4dabf7",
      badgeText: "#111315",
      palette: DARK_PALETTE,
    };
  }

  private resolveKind(): ThemeKind {
    if (this.preference !== "auto") {
      return this.preference;
    }
    if (this.vscodeKind) {
      return this.vscodeKind;
    }
    const domKind = detectDomVscodeKind();
    if (domKind) {
      return domKind;
    }
    return detectSystemKind();
  }

  private apply(): void {
    const nextKind = this.resolveKind();
    this.activeKind = nextKind;
    const root = document.documentElement;
    root.dataset.theme = nextKind;
    root.style.colorScheme = nextKind === "light" || nextKind === "hc-light" ? "light" : "dark";

    // Apply VS Code theme variables only when in 'auto' mode (or when matching kind)
    const useVsVars = this.preference === "auto" && this.vscodeKind !== null;
    for (const targetVar of Object.values(VSCODE_VAR_MAP)) {
      root.style.removeProperty(targetVar);
    }
    if (useVsVars) {
      for (const [vsVar, localVar] of Object.entries(VSCODE_VAR_MAP)) {
        const value = this.vscodeVars[vsVar];
        if (value) {
          root.style.setProperty(localVar, value);
        }
      }
      root.dataset.vscodeSynced = "true";
    } else {
      delete root.dataset.vscodeSynced;
    }

    const canvasTheme = this.getCanvasTheme();
    for (const listener of this.listeners) {
      listener(canvasTheme);
    }
  }

  private setupListeners(): void {
    // 1. Listen for postMessage from VS Code BackendPanel outer webview wrapper
    window.addEventListener("message", (event) => {
      const data = event.data;
      if (!data || typeof data !== "object") return;
      if (data.type === "jlinkd:vscode-theme") {
        const kind = parseThemeKind(data.kind);
        if (kind) {
          this.vscodeKind = kind;
        }
        if (typeof data.themeName === "string") {
          this.vscodeThemeName = data.themeName;
        }
        if (data.vars && typeof data.vars === "object") {
          this.vscodeVars = data.vars as Record<string, string>;
        }
        this.apply();
      }
    });

    // 2. Request theme from parent frame if embedded in an iframe (VS Code Webview)
    if (window.parent && window.parent !== window) {
      const requestTheme = () => {
        try {
          window.parent.postMessage({ type: "jlinkd:request-theme" }, "*");
        } catch {
          // Ignore cross-origin frame errors
        }
      };
      requestTheme();
      window.addEventListener("load", requestTheme, { once: true });
      // Also request when visibility changes (e.g. switching editor tabs in VS Code)
      document.addEventListener("visibilitychange", () => {
        if (!document.hidden) requestTheme();
      });
    }

    // 3. Observe direct DOM class changes if running directly inside a VS Code webview
    if (document.body) {
      const observer = new MutationObserver(() => {
        const domKind = detectDomVscodeKind();
        if (domKind && domKind !== this.vscodeKind) {
          this.vscodeKind = domKind;
          this.apply();
        }
      });
      observer.observe(document.body, {
        attributes: true,
        attributeFilter: ["class", "data-vscode-theme-kind", "data-vscode-theme-name"],
      });
    }

    // 4. Listen for OS theme changes when not overridden by VS Code
    const mediaLight = window.matchMedia?.("(prefers-color-scheme: light)");
    const mediaContrast = window.matchMedia?.("(prefers-contrast: more)");
    const onSystemChange = () => {
      if (this.preference === "auto" && !this.vscodeKind) {
        this.apply();
      }
    };
    mediaLight?.addEventListener?.("change", onSystemChange);
    mediaContrast?.addEventListener?.("change", onSystemChange);
  }
}
