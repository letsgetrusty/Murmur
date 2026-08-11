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

  // Progress fill (0..1): read-aloud progress while "reading", download
  // progress while "preparing"; cleared for every other state.
  const fill = pill.querySelector(".pill-fill");
  if (fill) {
    let p = 0;
    if (kind === "reading") p = payload.progress ?? 0;
    else if (kind === "preparing" && (payload.total ?? 0) > 0)
      p = (payload.downloaded ?? 0) / payload.total;
    fill.style.transform = `scaleX(${Math.max(0, Math.min(1, p))})`;
  }

  switch (kind) {
    case "preparing": {
      const total = payload.total ?? 0;
      const pct = total > 0 ? Math.round(((payload.downloaded ?? 0) / total) * 100) : null;
      label.textContent =
        pct != null ? `Downloading speech model… ${pct}%` : "Downloading speech model…";
      break;
    }
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
  // The recording dot pulses with your voice. Raw peak amplitude is usually low
  // (~0.05–0.3), so apply a perceptual curve (sqrt ≈ loudness) + gain so ordinary
  // speech clearly moves it, with a fast attack / slow release so the dot jumps
  // to sound and eases back instead of flickering.
  let level = 0;
  tauri.event.listen(EVENTS.AUDIO_LEVEL, (e) => {
    const raw = typeof e.payload === "number" ? e.payload : 0;
    const target = Math.min(1, Math.sqrt(raw) * 1.8);
    level = target > level ? target : level * 0.7 + target * 0.3;
    document.documentElement.style.setProperty("--audio-level", String(level));
  });
}

if (document.readyState === "loading") {
  window.addEventListener("DOMContentLoaded", init);
} else {
  init();
}
