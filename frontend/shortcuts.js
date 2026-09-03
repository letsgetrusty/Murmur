// Pure helpers for the keyboard-shortcut recorder — no DOM or Tauri deps, so
// they're unit-testable in isolation (see shortcuts.test.js).

// Render an accelerator string ("CmdOrCtrl+Shift+Space") with mac modifier
// glyphs; non-modifier tokens (letters, F-keys, "Space") pass through.
export function prettyShortcut(s) {
  return (s || "")
    .split("+")
    .map((t) => {
      switch (t) {
        case "CmdOrCtrl":
        case "Cmd":
        case "Command":
        case "Super":
          return "⌘";
        case "Ctrl":
        case "Control":
          return "⌃";
        case "Alt":
        case "Option":
          return "⌥";
        case "Shift":
          return "⇧";
        default:
          return t;
      }
    })
    .join("");
}

// Short label (mac glyphs) for each hold-to-talk dictate trigger. Shared by the
// Settings refine hint and the onboarding kbd label so they can't drift.
export const TRIGGER_LABEL = {
  Fn: "Fn",
  RightCtrl: "Right ⌃",
  RightAlt: "Right ⌥",
  RightCmd: "Right ⌘",
  Ctrl: "⌃",
  Alt: "⌥",
  Cmd: "⌘",
};

// Map a KeyboardEvent.code to the accelerator key token Tauri expects, or null
// for pure-modifier / unsupported codes (so the recorder keeps waiting).
export function codeToKey(code) {
  if (/^Key[A-Z]$/.test(code)) return code.slice(3);
  if (/^Digit[0-9]$/.test(code)) return code.slice(5);
  if (/^F([1-9]|1[0-9]|2[0-4])$/.test(code)) return code;
  const map = {
    Space: "Space",
    Enter: "Enter",
    Tab: "Tab",
    Backspace: "Backspace",
    Delete: "Delete",
    ArrowUp: "Up",
    ArrowDown: "Down",
    ArrowLeft: "Left",
    ArrowRight: "Right",
    Minus: "-",
    Equal: "=",
    BracketLeft: "[",
    BracketRight: "]",
    Backslash: "\\",
    Semicolon: ";",
    Quote: "'",
    Comma: ",",
    Period: ".",
    Slash: "/",
    Backquote: "`",
  };
  return map[code] || null; // pure modifier codes fall through to null
}
