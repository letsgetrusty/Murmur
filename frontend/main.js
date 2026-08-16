// Overlay state renderer. The Rust action router emits a `state` event on
// every transition (recording → transcribing → done/error). We just paint it.

import { EVENTS } from "./constants.js";

const LABELS = {
  idle: "Murmur",
  recording: "Recording…",
  transcribing: "Transcribing…",
  refining: "Refining…",
  reading: "Reading aloud…",
};

// Per-state leading glyph (design "1A"). Recording is a live waveform,
// transcribing + refining an accent spinner, the rest an SVG icon (accent, drawn
// with currentColor so per-state color overrides in styles.css apply). Idle →
// empty: the pill is hidden anyway.
const ICONS = {
  reading:
    '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M11 5 6 9H2v6h4l5 4V5z"/><path d="M15.5 8.5a5 5 0 0 1 0 7"/></svg>',
  preparing:
    '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M12 3v11m0 0 4-4m-4 4-4-4M5 21h14"/></svg>',
  done: '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.4" stroke-linecap="round" stroke-linejoin="round"><path d="M20 6 9 17l-5-5"/></svg>',
  error:
    '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M10.3 3.9 1.8 18a2 2 0 0 0 1.7 3h17a2 2 0 0 0 1.7-3L13.7 3.9a2 2 0 0 0-3.4 0z"/><path d="M12 9v4M12 17h.01"/></svg>',
};

function indicatorHTML(kind) {
  if (kind === "recording") return '<span class="wave"><i></i><i></i><i></i><i></i><i></i></span>';
  if (kind === "transcribing" || kind === "refining") return '<span class="spin"></span>';
  return ICONS[kind] ?? "";
}

let lastKind = null;

function applyState(payload) {
  const pill = document.querySelector(".pill");
  const label = pill?.querySelector(".label");
  if (!pill || !label) return;

  const kind = payload?.kind ?? "idle";
  pill.dataset.state = kind;

  // Rebuild the leading glyph only on an actual state change — the "reading"
  // state re-emits on every progress tick, and re-setting innerHTML would
  // restart its animation each time.
  if (kind !== lastKind) {
    const ind = pill.querySelector(".pill-ind");
    if (ind) ind.innerHTML = indicatorHTML(kind);
    lastKind = kind;
  }

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
      label.textContent = chars > 0 ? `Pasted · ${chars} chars` : "Done";
      break;
    }
    case "error": {
      label.textContent = payload.message ?? "Something went wrong";
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
  // Drive the recording waveform's bars straight from the mic level so it tracks
  // your actual voice. "audio:level" fires ~15 Hz (only while recording, when the
  // waveform is on screen) with a raw peak amplitude, usually low (~0.05–0.3);
  // curve it (sqrt ≈ loudness) + gain so ordinary speech clearly moves it, then
  // scroll it through a short history so the bars read as a live meter. The 12%
  // floor keeps a faint wave during silence.
  const history = new Array(5).fill(0);
  tauri.event.listen(EVENTS.AUDIO_LEVEL, (e) => {
    const raw = typeof e.payload === "number" ? e.payload : 0;
    const v = Math.min(1, Math.sqrt(raw) * 1.8);
    history.push(v);
    history.shift();
    const bars = document.querySelectorAll(".pill .wave i");
    for (let i = 0; i < bars.length; i++) {
      bars[i].style.height = `${(12 + (history[i] ?? 0) * 88).toFixed(1)}%`;
    }
  });
}

if (document.readyState === "loading") {
  window.addEventListener("DOMContentLoaded", init);
} else {
  init();
}
