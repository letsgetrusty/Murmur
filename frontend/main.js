// Overlay state renderer. The Rust action router emits a `state` event on
// every transition (recording → transcribing → done/error). We just paint it.

const LABELS = {
  idle: "murmur",
  recording: "Recording…",
  transcribing: "Transcribing…",
  refining: "Refining…",
  interpreting: "Interpreting…",
};

function applyState(payload) {
  const pill = document.querySelector(".pill");
  const label = pill?.querySelector(".label");
  if (!pill || !label) return;

  const kind = payload?.kind ?? "idle";
  pill.dataset.state = kind;

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
      label.textContent = LABELS[kind] ?? "murmur";
  }
}

function init() {
  applyState({ kind: "idle" });

  const tauri = window.__TAURI__;
  if (!tauri?.event?.listen) {
    console.warn("Tauri event API unavailable; overlay state will not update.");
    return;
  }
  tauri.event.listen("state", (e) => applyState(e.payload));
  tauri.event.listen("audio:level", (e) => {
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
