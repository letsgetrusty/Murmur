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
let activeCancel = null;

// Cancel any in-progress capture (restoring its label). For flows that mutate
// bindings out from under an open recorder, e.g. "Reset to defaults".
export function cancelActiveRecorder() {
  if (activeCancel) activeCancel();
}

// Turn a `.recorder` button into a live chord capturer.
// - `getCurrent()` returns the current shortcut string (to render + restore on
//   cancel).
// - `onCapture(shortcut)` persists a validated combo; if its promise rejects,
//   the label reverts and briefly flashes invalid.
// - `onOpen()` / `onClose()` (optional) fire when capture starts / ends — used
//   to suspend + resume the global shortcuts so a currently-bound combo reaches
//   the recorder instead of firing its action. `onClose` runs only *after* a
//   successful capture is applied, so the resume re-registers the new binding.
// Returns `{ render, cancel }` — `render()` re-syncs the label from `getCurrent()`.
export function bindRecorder(button, { getCurrent, onCapture, onOpen, onClose }) {
  const render = () => {
    button.textContent = prettyShortcut(getCurrent());
  };

  // Stop listening + drop the recording styling. Does NOT resume shortcuts —
  // callers decide when (after a rebind vs. immediately on cancel).
  function teardown() {
    window.removeEventListener("keydown", onKey, true);
    button.classList.remove("recording", "invalid");
    if (activeCancel === cancel) activeCancel = null;
  }

  // Abandon capture: restore the label and resume shortcuts.
  function cancel() {
    teardown();
    render();
    onClose?.();
  }

  async function onKey(e) {
    e.preventDefault();
    e.stopPropagation();
    if (e.key === "Escape") {
      cancel(); // reverting the label is the cancel confirmation
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
    teardown();
    button.textContent = prettyShortcut(shortcut); // label change confirms success
    try {
      await onCapture(shortcut);
    } catch (_) {
      render();
      button.classList.add("invalid");
      setTimeout(() => button.classList.remove("invalid"), 1800);
    } finally {
      onClose?.(); // resume only after the new binding is registered
    }
  }

  button.addEventListener("click", () => {
    if (activeCancel) activeCancel();
    activeCancel = cancel;
    button.classList.remove("invalid");
    button.classList.add("recording");
    button.textContent = "Press keys…";
    onOpen?.(); // suspend shortcuts so the keys reach us
    window.addEventListener("keydown", onKey, true);
  });

  render();
  return { render };
}
