import { describe, it, expect } from "vitest";
import { prettyShortcut, codeToKey } from "./shortcuts.js";

describe("prettyShortcut", () => {
  it("maps modifier tokens to mac glyphs", () => {
    expect(prettyShortcut("CmdOrCtrl+Shift+Space")).toBe("⌘⇧Space");
    expect(prettyShortcut("Ctrl+Alt+Delete")).toBe("⌃⌥Delete");
    expect(prettyShortcut("Command+Super+Cmd")).toBe("⌘⌘⌘");
    expect(prettyShortcut("Option+Control")).toBe("⌥⌃");
  });

  it("passes non-modifier tokens through unchanged", () => {
    expect(prettyShortcut("F5")).toBe("F5");
    expect(prettyShortcut("A")).toBe("A");
  });

  it("handles empty / nullish input", () => {
    expect(prettyShortcut("")).toBe("");
    expect(prettyShortcut(null)).toBe("");
    expect(prettyShortcut(undefined)).toBe("");
  });
});

describe("codeToKey", () => {
  it("maps letter and digit codes", () => {
    expect(codeToKey("KeyA")).toBe("A");
    expect(codeToKey("KeyZ")).toBe("Z");
    expect(codeToKey("Digit0")).toBe("0");
    expect(codeToKey("Digit9")).toBe("9");
  });

  it("keeps valid function keys (F1–F24)", () => {
    expect(codeToKey("F1")).toBe("F1");
    expect(codeToKey("F12")).toBe("F12");
    expect(codeToKey("F24")).toBe("F24");
  });

  it("maps named and punctuation codes", () => {
    expect(codeToKey("Space")).toBe("Space");
    expect(codeToKey("ArrowUp")).toBe("Up");
    expect(codeToKey("Minus")).toBe("-");
    expect(codeToKey("Slash")).toBe("/");
    expect(codeToKey("Backquote")).toBe("`");
  });

  it("returns null for pure modifiers and unsupported codes", () => {
    expect(codeToKey("ShiftLeft")).toBeNull();
    expect(codeToKey("ControlRight")).toBeNull();
    expect(codeToKey("MetaLeft")).toBeNull();
    expect(codeToKey("F25")).toBeNull();
    expect(codeToKey("")).toBeNull();
  });
});
