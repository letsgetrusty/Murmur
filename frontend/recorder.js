// Shared "press-your-combo" hotkey recorder, used by the Settings window and the
// onboarding flow so both capture + validate chords identically. Pure key logic
// lives in shortcuts.js; this adds the DOM interaction.

import { prettyShortcut, codeToKey } from "./shortcuts.js";

// Combos we never let a global shortcut take: core macOS editing keys and the
// ones Murmur synthesizes for paste/copy — binding those would break dictation.
// Mirrors is_reserved_shortcut() in hotkeys.rs (the backend rejects them too).
export const RESERVED_SHORTCUTS = new Set([
  "Cmd+V",
  "Cmd+C",
  "Cmd+X",
  "Cmd+A",
  "Cmd+Z",
  "Cmd+Q",
  "Cmd+W",
]);

// One recorder captures at a time across the whole window.
let activeStop = null;

// Cancel any in-progress capture (restoring its label). For flows that mutate
// bindings out from under an open recorder, e.g. "Reset to defaults".
export function cancelActiveRecorder() {
  if (activeStop) activeStop(true);
}

// Turn a `.recorder` button into a live chord capturer.
// - `getCurrent()` returns the current shortcut string (to render + restore on
//   cancel).
// - `onCapture(shortcut)` persists a validated combo; if its promise rejects,
//   the label reverts and briefly flashes invalid.
// Returns `{ render, stop }` — `render()` re-syncs the label from `getCurrent()`.
export function bindRecorder(button, { getCurrent, onCapture }) {
  const render = () => {
    button.textContent = prettyShortcut(getCurrent());
  };

  function stop(restore) {
    window.removeEventListener("keydown", onKey, true);
    button.classList.remove("recording", "invalid");
    if (restore) render();
    if (activeStop === stop) activeStop = null;
  }

  async function onKey(e) {
    e.preventDefault();
    e.stopPropagation();
    if (e.key === "Escape") {
      stop(true); // reverting the label is the cancel confirmation
      return;
    }
    const key = codeToKey(e.code);
    if (!key) return; // still waiting for a non-modifier key

    const mods = [];
    if (e.metaKey) mods.push("Cmd");
    if (e.ctrlKey) mods.push("Ctrl");
    if (e.altKey) mods.push("Alt");
    if (e.shiftKey) mods.push("Shift");
    if (mods.length === 0) {
      button.classList.add("invalid");
      button.textContent = "Hold a modifier ⌘⌃⌥⇧";
      return;
    }

    const shortcut = [...mods, key].join("+");
    if (RESERVED_SHORTCUTS.has(shortcut)) {
      button.classList.add("invalid");
      button.textContent = "Reserved — pick another";
      return;
    }
    stop(false);
    button.textContent = prettyShortcut(shortcut); // label change confirms success
    try {
      await onCapture(shortcut);
    } catch (_) {
      render();
      button.classList.add("invalid");
      setTimeout(() => button.classList.remove("invalid"), 1800);
    }
  }

  button.addEventListener("click", () => {
    if (activeStop) activeStop(true);
    activeStop = stop;
    button.classList.remove("invalid");
    button.classList.add("recording");
    button.textContent = "Press keys…";
    window.addEventListener("keydown", onKey, true);
  });

  render();
  return { render, stop };
}
