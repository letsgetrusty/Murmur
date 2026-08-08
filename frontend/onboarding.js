// First-run onboarding. A small stepper that walks the user through
// permissions and the one-time model downloads, then relaunches (if needed) so
// the Fn tap picks up a freshly granted Accessibility permission.

import { EVENTS, CMD, DOWNLOAD } from "./constants.js";

const invoke = window.__TAURI__?.core?.invoke;
const listen = window.__TAURI__?.event?.listen;
const currentWindow = window.__TAURI__?.window?.getCurrentWindow?.();

const STEPS = 4;
let step = 0;

// True once we observe Accessibility flip to granted while the app is running —
// that grant only takes effect for the Fn tap after a relaunch.
let accessibilityGranted = false;

const $ = (sel, root = document) => root.querySelector(sel);
const $$ = (sel, root = document) => [...root.querySelectorAll(sel)];
const stepEl = (n) => $(`.ob-step[data-step="${n}"]`);

// --- Stepper -----------------------------------------------------------------

function buildDots() {
  const host = $("#ob-dots");
  for (let i = 0; i < STEPS; i++) {
    const d = document.createElement("span");
    d.className = "dot";
    host.appendChild(d);
  }
}

function renderDots() {
  $$("#ob-dots .dot").forEach((d, i) => {
    d.classList.toggle("active", i === step);
    d.classList.toggle("done", i < step);
  });
}

// Step 2 is "Downloading models".
const DOWNLOAD_STEP = 2;

function goTo(n) {
  step = Math.max(0, Math.min(STEPS - 1, n));
  $$(".ob-step").forEach((s) => {
    s.hidden = Number(s.dataset.step) !== step;
  });
  renderDots();
  // Start the Kokoro download when the user reaches the downloads step (if the
  // neural voice is kept), so its bar fills alongside Whisper/Qwen.
  if (step === DOWNLOAD_STEP) maybeStartNeural();
}

// --- Permissions -------------------------------------------------------------

function setBadge(rowId, text, kind) {
  const badge = $(`#${rowId} [data-badge]`);
  if (!badge) return;
  badge.textContent = text;
  badge.className = `perm-badge${kind ? " " + kind : ""}`;
}

function renderPermissions(status) {
  // Accessibility
  if (status.accessibility) {
    accessibilityGranted = true;
    setBadge("perm-ax", "Granted", "ok");
    $("#perm-ax [data-open-ax]").disabled = true;
  } else {
    setBadge("perm-ax", "Not granted", "warn");
    $("#perm-ax [data-open-ax]").disabled = false;
  }
  // Relaunch note appears once we know a grant happened this session.
  $("#ax-relaunch-note").hidden = !accessibilityGranted;

  // Microphone: 0 notDetermined, 1 restricted, 2 denied, 3 authorized
  const mic = status.microphone;
  const micBtn = $("#perm-mic [data-mic-action]");
  if (mic === 3) {
    setBadge("perm-mic", "Granted", "ok");
    micBtn.disabled = true;
    micBtn.textContent = "Enabled";
  } else if (mic === 2 || mic === 1) {
    setBadge("perm-mic", "Denied", "warn");
    micBtn.disabled = false;
    micBtn.textContent = "Open Settings";
    micBtn.dataset.mode = "settings";
  } else {
    setBadge("perm-mic", "Not set", "");
    micBtn.disabled = false;
    micBtn.textContent = "Enable";
    micBtn.dataset.mode = "request";
  }
}

async function refreshStatus() {
  if (!invoke) return;
  try {
    const status = await invoke(CMD.ONBOARDING_STATUS);
    renderPermissions(status);
    // Seed the download bars for anything already on disk.
    if (status.whisper_ready) markDownloadDone(DOWNLOAD.WHISPER);
    if (status.llm_ready) markDownloadDone(DOWNLOAD.LLM);
    if (status.kokoro_ready) markDownloadDone(DOWNLOAD.KOKORO);
  } catch (e) {
    /* transient; polled again shortly */
  }
}

// --- Model downloads ---------------------------------------------------------

const DL_EL = {
  [DOWNLOAD.WHISPER]: "dl-whisper",
  [DOWNLOAD.LLM]: "dl-llm",
  [DOWNLOAD.KOKORO]: "dl-kokoro",
};
const dlDone = {
  [DOWNLOAD.WHISPER]: false,
  [DOWNLOAD.LLM]: false,
  [DOWNLOAD.KOKORO]: false,
};

// The Kokoro download is opt-out: kicked off (once) only when the neural-voice
// box is kept, so opting out never fetches the ~310 MB model.
let neuralStarted = false;
function maybeStartNeural() {
  const on = $("#ob-neural")?.checked;
  if (!on || neuralStarted || !invoke) return;
  neuralStarted = true;
  invoke(CMD.DOWNLOAD_NEURAL_VOICE).catch(() => {
    neuralStarted = false; // let a later attempt retry
  });
}

function markDownloadDone(id) {
  dlDone[id] = true;
  const row = $(`#${DL_EL[id]}`);
  if (!row) return;
  const fill = $("[data-fill]", row);
  fill.style.width = "100%";
  fill.classList.add("done");
  fill.classList.remove("failed");
  $("[data-pct]", row).textContent = "Ready ✓";
}

function renderDownload({ id, downloaded, total, failed }) {
  const row = $(`#${DL_EL[id]}`);
  if (!row) return;
  const fill = $("[data-fill]", row);
  const pct = $("[data-pct]", row);
  if (failed) {
    fill.classList.add("failed");
    pct.textContent = "Failed — retries on next launch";
    return;
  }
  if (total > 0) {
    const frac = Math.min(1, downloaded / total);
    fill.style.width = `${(frac * 100).toFixed(1)}%`;
    if (frac >= 1) {
      markDownloadDone(id);
    } else {
      pct.textContent = `${(frac * 100).toFixed(0)}% · ${fmtMB(downloaded)} / ${fmtMB(total)}`;
    }
  } else {
    // No Content-Length — show bytes pulled so far.
    pct.textContent = fmtMB(downloaded);
  }
}

function fmtMB(bytes) {
  const mb = bytes / (1024 * 1024);
  return mb >= 1024 ? `${(mb / 1024).toFixed(2)} GB` : `${Math.round(mb)} MB`;
}

// --- Finish ------------------------------------------------------------------

async function finish() {
  const btn = $("#ob-finish");
  btn.disabled = true;
  // Persist the read-aloud voice choice. When kept (default), the ~310 MB Kokoro
  // model downloads via the startup prefetch after the relaunch below; when
  // unchecked, the backend stays on the built-in macOS voice and never fetches it.
  const neural = $("#ob-neural");
  if (neural) {
    try {
      await invoke(CMD.SET_NEURAL_VOICE, { enabled: neural.checked });
    } catch (_) {
      /* non-fatal; default (neural) stands */
    }
  }
  try {
    await invoke(CMD.FINISH_ONBOARDING);
  } catch (e) {
    btn.disabled = false;
    return;
  }
  // A grant made while running only reaches the Fn tap after a relaunch.
  if (accessibilityGranted) {
    await invoke(CMD.RELAUNCH_APP);
  } else if (currentWindow) {
    await currentWindow.close();
  }
}

// --- Wiring ------------------------------------------------------------------

function init() {
  buildDots();
  goTo(0);

  $$("[data-next]").forEach((b) => b.addEventListener("click", () => goTo(step + 1)));
  $$("[data-back]").forEach((b) => b.addEventListener("click", () => goTo(step - 1)));

  $("[data-open-ax]").addEventListener("click", () => {
    invoke?.(CMD.OPEN_ACCESSIBILITY_SETTINGS);
  });

  $("[data-mic-action]").addEventListener("click", async (e) => {
    const btn = e.currentTarget;
    if (btn.dataset.mode === "settings") {
      invoke?.(CMD.OPEN_MICROPHONE_SETTINGS);
      return;
    }
    btn.disabled = true;
    btn.textContent = "Waiting…";
    try {
      const status = await invoke(CMD.REQUEST_MICROPHONE);
      renderPermissions({ accessibility: accessibilityGranted, microphone: status });
    } catch (_) {
      btn.disabled = false;
      btn.textContent = "Enable";
    }
  });

  $("#ob-finish").addEventListener("click", finish);

  // Neural-voice opt-out: persist the choice, hide its download row when off,
  // and start the download when turned (back) on.
  const neural = $("#ob-neural");
  neural?.addEventListener("change", () => {
    invoke?.(CMD.SET_NEURAL_VOICE, { enabled: neural.checked });
    const row = $("#dl-kokoro");
    if (row) row.hidden = !neural.checked;
    if (neural.checked) maybeStartNeural();
  });

  // Model-download progress from the backend's prefetch.
  listen?.(EVENTS.MODEL_DOWNLOAD, (e) => renderDownload(e.payload));

  // Permissions change in System Settings (outside the app), so poll.
  refreshStatus();
  setInterval(refreshStatus, 1500);
}

if (document.readyState === "loading") {
  window.addEventListener("DOMContentLoaded", init);
} else {
  init();
}
