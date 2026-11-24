import { describe, expect, it } from "vitest";
import { KEYBOARD_HELP, MOUSE_HELP, resolveShortcut, type KeyEventLike } from "./keymap";

function key(
  k: string,
  opts: Partial<Omit<KeyEventLike, "key">> = {},
): KeyEventLike {
  return {
    key: k,
    ctrlKey: false,
    metaKey: false,
    shiftKey: false,
    altKey: false,
    isInputFocused: false,
    ...opts,
  };
}

describe("timeline keymap", () => {
  it("resolves navigation, zoom, cursor and signal shortcuts", () => {
    expect(resolveShortcut(key(" "))).toBe("toggleFollow");
    expect(resolveShortcut(key("ArrowLeft"))).toBe("panLeftDiv");
    expect(resolveShortcut(key("ArrowRight"))).toBe("panRightDiv");
    expect(resolveShortcut(key("ArrowLeft", { ctrlKey: true }))).toBe("panLeftScreen");
    expect(resolveShortcut(key("ArrowRight", { ctrlKey: true }))).toBe("panRightScreen");
    expect(resolveShortcut(key("Home"))).toBe("jumpStart");
    expect(resolveShortcut(key("End"))).toBe("jumpEnd");
    expect(resolveShortcut(key("+"))).toBe("zoomIn");
    expect(resolveShortcut(key("="))).toBe("zoomIn");
    expect(resolveShortcut(key("-"))).toBe("zoomOut");
    expect(resolveShortcut(key("0", { ctrlKey: true }))).toBe("fitAll");
    expect(resolveShortcut(key("ArrowUp"))).toBe("selectPrevTrack");
    expect(resolveShortcut(key("ArrowDown"))).toBe("selectNextTrack");
    expect(resolveShortcut(key("Delete"))).toBe("deleteSelectedSignal");
    expect(resolveShortcut(key("a"))).toBe("placeCursorA");
    expect(resolveShortcut(key("B"))).toBe("placeCursorB");
    expect(resolveShortcut(key("Escape"))).toBe("clearCursors");
    expect(resolveShortcut(key("h"))).toBe("toggleHideSignal");
    expect(resolveShortcut(key("s"))).toBe("toggleSoloSignal");
    expect(resolveShortcut(key("y"))).toBe("autoScaleTrackY");
    expect(resolveShortcut(key("/"))).toBe("focusExpr");
    expect(resolveShortcut(key("?"))).toBe("toggleHelp");
  });

  it("ignores shortcuts while typing in inputs except Escape", () => {
    expect(resolveShortcut(key(" ", { isInputFocused: true }))).toBeNull();
    expect(resolveShortcut(key("a", { isInputFocused: true }))).toBeNull();
    expect(resolveShortcut(key("Delete", { isInputFocused: true }))).toBeNull();
    expect(resolveShortcut(key("Escape", { isInputFocused: true }))).toBe("blurInput");
  });

  it("ignores Alt-modified browser shortcuts and documents all controls", () => {
    expect(resolveShortcut(key("ArrowLeft", { altKey: true }))).toBeNull();
    expect(KEYBOARD_HELP.length).toBeGreaterThanOrEqual(14);
    expect(MOUSE_HELP.length).toBeGreaterThanOrEqual(9);
  });
});
