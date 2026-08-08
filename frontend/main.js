// Overlay state renderer. The Rust action router emits a `state` event on
// every transition (recording → transcribing → done/error). We just paint it.

import { EVENTS } from "./constants.js";

const LABELS = {
  idle: "Murmur",
  recording: "Recording…",
  transcribing: "Transcribing…",
  refining: "Refining…",
  reading: "Reading…",
};

function applyState(payload) {
  const pill = document.querySelector(".pill");
  const label = pill?.querySelector(".label");
  if (!pill || !label) return;

  const kind = payload?.kind ?? "idle";
  pill.dataset.state = kind;

  // Read-aloud progress fill (0..1); cleared for every other state.
  const fill = pill.querySelector(".pill-fill");
  if (fill) {
    const p = kind === "reading" ? (payload.progress ?? 0) : 0;
    fill.style.transform = `scaleX(${Math.max(0, Math.min(1, p))})`;
  }

  switch (kind) {
    case "done": {
      const chars = payload.chars ?? 0;
      label.textContent = chars > 0 ? `✓ pasted (${chars} chars)` : "✓ done";
      break;
    }
    case "error": {
      label.textContent = `✗ ${payload.message ?? "error"}`;
      break;
    }
    default:
      label.textContent = LABELS[kind] ?? "Murmur";
  }
}

function init() {
  applyState({ kind: "idle" });

  const tauri = window.__TAURI__;
  if (!tauri?.event?.listen) {
    console.warn("Tauri event API unavailable; overlay state will not update.");
    return;
  }
  tauri.event.listen(EVENTS.STATE, (e) => applyState(e.payload));
  tauri.event.listen(EVENTS.AUDIO_LEVEL, (e) => {
    // payload is a 0..1 peak amplitude from the capture thread.
    const v = typeof e.payload === "number" ? e.payload : 0;
    document.documentElement.style.setProperty("--audio-level", String(v));
  });
}

if (document.readyState === "loading") {
  window.addEventListener("DOMContentLoaded", init);
} else {
  init();
}
