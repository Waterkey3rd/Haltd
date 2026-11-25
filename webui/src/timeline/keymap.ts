export type ShortcutAction =
  | "toggleFollow"
  | "panLeftDiv"
  | "panRightDiv"
  | "panLeftScreen"
  | "panRightScreen"
  | "jumpStart"
  | "jumpEnd"
  | "zoomIn"
  | "zoomOut"
  | "fitAll"
  | "selectPrevTrack"
  | "selectNextTrack"
  | "deleteSelectedSignal"
  | "placeCursorA"
  | "placeCursorB"
  | "clearCursors"
  | "toggleHideSignal"
  | "toggleSoloSignal"
  | "autoScaleTrackY"
  | "focusExpr"
  | "toggleHelp"
  | "blurInput";

export interface KeyEventLike {
  key: string;
  ctrlKey: boolean;
  metaKey: boolean;
  shiftKey: boolean;
  altKey: boolean;
  isInputFocused: boolean;
}

export interface ShortcutHelpItem {
  keys: string;
  description: string;
}

export const KEYBOARD_HELP: ShortcutHelpItem[] = [
  { keys: "Space", description: "Toggle live following / paused" },
  { keys: "← / →", description: "Pan backward / forward by 1 division" },
  { keys: "Ctrl+← / Ctrl+→", description: "Pan backward / forward by 1 screen" },
  { keys: "Home / End", description: "Jump to earliest data / latest (resume follow)" },
  { keys: "+ / -", description: "Zoom time division in / out" },
  { keys: "Ctrl+0", description: "Zoom to fit available history" },
  { keys: "↑ / ↓", description: "Select previous / next track" },
  { keys: "Del", description: "Remove selected signal" },
  { keys: "A / B", description: "Place measurement cursor A / B at hover position" },
  { keys: "Esc", description: "Clear cursors & close popups" },
  { keys: "H", description: "Hide / show selected signal" },
  { keys: "S", description: "Solo / unsolo selected signal" },
  { keys: "Y", description: "Auto-scale Y axis of selected track" },
  { keys: "/", description: "Focus expression input box" },
  { keys: "?", description: "Toggle this keyboard shortcuts help" },
];

export const MOUSE_HELP: ShortcutHelpItem[] = [
  { keys: "Wheel", description: "Scroll horizontally in time" },
  { keys: "Shift + Wheel", description: "Zoom Y axis of track under mouse" },
  { keys: "Ctrl + Wheel", description: "Zoom time axis centered at mouse cursor" },
  { keys: "Left / Middle Drag", description: "Pan X & Y freely (vertical-only drag keeps live follow)" },
  { keys: "Shift + Left Drag", description: "Pan Y axis only (keeps live follow)" },
  { keys: "Ctrl + Left Drag", description: "Box-select time region to zoom" },
  { keys: "Click / Double-Click", description: "Place Sample Cursor (S), shows values and Δt to hover" },
  { keys: "Drag S / A / B / Pin", description: "Move Sample / A / B cursor or Pin handle on axis or plot" },
  { keys: "Right-Click", description: "Track & signal context menu (fit, Y auto, cursors, pins, move, CSV)" },
  { keys: "Drag Legend Item", description: "Move signal to another track or drop on blank area for new track" },
  { keys: "Drag Track Border", description: "Resize track height" },
];

export function isInputElement(target: EventTarget | null): boolean {
  if (!target || typeof target !== "object") {
    return false;
  }
  const tag = (target as { tagName?: string }).tagName?.toUpperCase();
  return tag === "INPUT" || tag === "SELECT" || tag === "TEXTAREA";
}

export function resolveShortcut(ev: KeyEventLike): ShortcutAction | null {
  if (ev.isInputFocused) {
    return ev.key === "Escape" ? "blurInput" : null;
  }
  if (ev.altKey) {
    return null;
  }

  const ctrl = ev.ctrlKey || ev.metaKey;
  if (ctrl) {
    switch (ev.key) {
      case "ArrowLeft":
        return "panLeftScreen";
      case "ArrowRight":
        return "panRightScreen";
      case "0":
        return "fitAll";
      case "+":
      case "=":
        return "zoomIn";
      case "-":
      case "_":
        return "zoomOut";
      default:
        return null;
    }
  }

  switch (ev.key) {
    case " ":
      return "toggleFollow";
    case "ArrowLeft":
      return "panLeftDiv";
    case "ArrowRight":
      return "panRightDiv";
    case "Home":
      return "jumpStart";
    case "End":
      return "jumpEnd";
    case "+":
    case "=":
      return "zoomIn";
    case "-":
    case "_":
      return "zoomOut";
    case "ArrowUp":
      return "selectPrevTrack";
    case "ArrowDown":
      return "selectNextTrack";
    case "Delete":
    case "Backspace":
      return "deleteSelectedSignal";
    case "a":
    case "A":
      return "placeCursorA";
    case "b":
    case "B":
      return "placeCursorB";
    case "Escape":
      return "clearCursors";
    case "h":
    case "H":
      return "toggleHideSignal";
    case "s":
    case "S":
      return "toggleSoloSignal";
    case "y":
    case "Y":
      return "autoScaleTrackY";
    case "/":
      return "focusExpr";
    case "?":
      return "toggleHelp";
    default:
      return null;
  }
}
