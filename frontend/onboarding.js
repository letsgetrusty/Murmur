// First-run onboarding. A small stepper that walks the user through
// permissions and the one-time model downloads, then relaunches (if needed) so
// the Fn tap picks up a freshly granted Accessibility permission.

import { EVENTS, CMD, DOWNLOAD } from "./constants.js";

const invoke = window.__TAURI__?.core?.invoke;
const listen = window.__TAURI__?.event?.listen;

const STEPS = 5;
let step = 0;

// True once we observe Accessibility currently granted (sticky).
let accessibilityGranted = false;

// Whether Accessibility was ALREADY granted on the first status poll. The Fn tap
// installs at startup gated on Accessibility, so a grant made *during* onboarding
// only activates the tap after a relaunch — but if it was already granted at
// launch, the tap is already live and no relaunch is needed. `null` until the
// first poll observes it.
let accessibilityGrantedAtStart = null;

// True while a microphone-permission request is awaiting the user's answer. The
// request now blocks until they respond, so the 1.5s status poll must not reset
// the button out from under the "Waiting…" state while the dialog is up.
let micRequestPending = false;

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

// Step 2 is "Downloading models"; step 3 is the "Try it" first-success test.
const DOWNLOAD_STEP = 2;
const TRY_STEP = 3;

function goTo(n) {
  step = Math.max(0, Math.min(STEPS - 1, n));
  $$(".ob-step").forEach((s) => {
    s.hidden = Number(s.dataset.step) !== step;
  });
  renderDots();
  // Start the Kokoro download when the user reaches the downloads step, so its
  // bar fills alongside Whisper/Qwen.
  if (step === DOWNLOAD_STEP) startNeural();
  // Reflect speech-model readiness on the "Hold to talk" button when we land
  // on the Try-it step (it can't record until Whisper is on disk).
  if (step === TRY_STEP) updateTryStep();
}

// --- Permissions -------------------------------------------------------------

function setBadge(rowId, text, kind) {
  const badge = $(`#${rowId} [data-badge]`);
  if (!badge) return;
  badge.textContent = text;
  badge.className = `perm-badge${kind ? " " + kind : ""}`;
}

// A relaunch is needed only when Accessibility was granted during this session
// (off at start, on now) — that's what arms the startup-installed Fn tap.
function needsRelaunch() {
  return accessibilityGranted && accessibilityGrantedAtStart === false;
}

function renderPermissions(status) {
  // Capture the initial Accessibility state on the first poll, so we can tell a
  // fresh grant (needs a relaunch to arm the Fn tap) from one already in effect.
  if (accessibilityGrantedAtStart === null) {
    accessibilityGrantedAtStart = status.accessibility;
  }
  // Accessibility
  if (status.accessibility) {
    accessibilityGranted = true;
    setBadge("perm-ax", "Granted", "ok");
    $("#perm-ax [data-open-ax]").disabled = true;
  } else {
    setBadge("perm-ax", "Not granted", "warn");
    $("#perm-ax [data-open-ax]").disabled = false;
  }
  // Relaunch note (and the Finish-time relaunch) apply only to a grant made
  // *this session* — if Accessibility was already on at launch, nothing to do.
  $("#ax-relaunch-note").hidden = !needsRelaunch();

  // Microphone: 0 notDetermined, 1 restricted, 2 denied, 3 authorized
  // While a request is in flight the click handler owns the button (showing
  // "Waiting…"); don't let the status poll clobber it back to "Enable".
  if (micRequestPending) return;
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
    // Reflect speech-model readiness on the Finish button (gated on Whisper).
    updateFinish();
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

// Kokoro is the default read-aloud voice, so fetch it (once) when the user reaches
// the downloads step. They can switch to the built-in macOS voice later in
// Settings; that never needs this ~310 MB model.
let neuralStarted = false;
function startNeural() {
  if (neuralStarted || !invoke) return;
  neuralStarted = true;
  invoke(CMD.DOWNLOAD_NEURAL_VOICE).catch(() => {
    neuralStarted = false; // let a later attempt retry
  });
}

// Whisper is the one model dictation can't work without, so the Finish button
// waits for it — nobody exits onboarding into a speech-to-text feature that
// silently does nothing. Qwen/Kokoro keep downloading in the background and
// self-heal, so they don't gate finishing.
const whisper = { ready: false, failed: false, downloaded: 0, total: 0 };

function updateFinish() {
  const btn = $("#ob-finish");
  if (!btn) return;
  if (!invoke || whisper.ready) {
    // Ready — or IPC unavailable, in which case never trap the user.
    btn.disabled = false;
    btn.textContent = "Finish";
  } else if (whisper.failed) {
    // Offline / download error — let them out; dictation self-heals on retry.
    btn.disabled = false;
    btn.textContent = "Finish anyway";
  } else {
    btn.disabled = true;
    const pct =
      whisper.total > 0 ? ` ${Math.round((whisper.downloaded / whisper.total) * 100)}%` : "…";
    btn.textContent = `Downloading speech model${pct}`;
  }
}

// Show/hide a row's Retry button (shown only when its download has failed).
function setRetry(id, show) {
  const row = $(`#${DL_EL[id]}`);
  const btn = row && $("[data-retry]", row);
  if (btn) btn.hidden = !show;
}

function markDownloadDone(id) {
  dlDone[id] = true;
  if (id === DOWNLOAD.WHISPER) {
    whisper.ready = true;
    whisper.failed = false;
    updateFinish();
    updateTryStep(); // Whisper just landed — the "Try it" mic can go live
  }
  const row = $(`#${DL_EL[id]}`);
  if (!row) return;
  const fill = $("[data-fill]", row);
  fill.style.width = "100%";
  fill.classList.add("done");
  fill.classList.remove("failed");
  setRetry(id, false);
  $("[data-pct]", row).textContent = "Ready ✓";
}

function renderDownload({ id, downloaded, total, failed }) {
  const row = $(`#${DL_EL[id]}`);
  if (!row) return;
  // Keep the Finish gate in sync with Whisper's live progress.
  if (id === DOWNLOAD.WHISPER) {
    whisper.downloaded = downloaded;
    whisper.total = total;
    whisper.failed = failed;
    updateFinish();
  }
  const fill = $("[data-fill]", row);
  const pct = $("[data-pct]", row);
  if (failed) {
    fill.classList.add("failed");
    pct.textContent = "Download failed";
    setRetry(id, true);
    return;
  }
  // Any progress means a (re)try is underway — hide the Retry button.
  setRetry(id, false);
  fill.classList.remove("failed");
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

// --- Try it (guided first success) -------------------------------------------

let tryRecording = false; // mic held down, capturing
let tryBusy = false; // released, awaiting the transcript

// Enable "Hold to talk" only once Whisper is on disk — recording before then
// would transcribe to nothing and read as a failure that isn't one.
function updateTryStep() {
  const btn = $("#try-mic");
  const label = $("#try-mic-label");
  if (!btn || !label || tryRecording || tryBusy) return;
  if (!invoke || whisper.ready) {
    btn.disabled = false;
    label.textContent = "Hold to talk";
  } else {
    btn.disabled = true;
    label.textContent = "Preparing speech model…";
  }
}

async function tryStart() {
  const btn = $("#try-mic");
  if (!btn || btn.disabled || tryRecording || tryBusy) return;
  tryRecording = true;
  btn.classList.add("recording");
  $("#try-mic-label").textContent = "Listening… release to stop";
  $("#try-result").hidden = true;
  try {
    await invoke(CMD.TEST_DICTATION_START);
  } catch (_) {
    tryRecording = false;
    btn.classList.remove("recording");
    updateTryStep();
  }
}

async function tryStop() {
  if (!tryRecording) return;
  tryRecording = false;
  tryBusy = true;
  const btn = $("#try-mic");
  btn.classList.remove("recording");
  btn.disabled = true;
  $("#try-mic-label").textContent = "Transcribing…";
  try {
    // The transcript comes back via the TEST_DICTATION_RESULT event.
    await invoke(CMD.TEST_DICTATION_STOP);
  } catch (_) {
    tryBusy = false;
    updateTryStep();
  }
}

function renderTryResult({ text, heard_audio }) {
  tryBusy = false;
  const result = $("#try-result");
  const transcript = $("#try-transcript");
  const status = $("#try-status");
  updateTryStep();
  result.hidden = false;
  result.classList.remove("ok", "warn");
  if (heard_audio && text) {
    transcript.textContent = `“${text}”`;
    status.textContent = "Heard you clearly — transcribed on-device. ✓";
    result.classList.add("ok");
  } else if (!heard_audio) {
    transcript.textContent = "";
    status.textContent =
      "We couldn't hear anything. Check your mic is connected and Murmur has Microphone access (go Back a step), then try again.";
    result.classList.add("warn");
  } else {
    transcript.textContent = "";
    status.textContent = "We didn't catch any words — try again, a little louder and clearer.";
    result.classList.add("warn");
  }
}

// --- Finish ------------------------------------------------------------------

async function finish() {
  const btn = $("#ob-finish");
  btn.disabled = true;
  // Read-aloud defaults to Kokoro (already the config default); it's changed in
  // Settings, not here. The model keeps downloading via the startup prefetch
  // after the relaunch/close below.
  try {
    await invoke(CMD.FINISH_ONBOARDING);
  } catch (e) {
    btn.disabled = false;
    return;
  }
  // Only relaunch when Accessibility was granted *this session* — that's the one
  // case the startup-installed Fn tap needs a restart to pick up. Otherwise just
  // close the onboarding window (no jarring, pointless relaunch).
  if (needsRelaunch()) {
    await invoke(CMD.RELAUNCH_APP);
  } else {
    await invoke(CMD.CLOSE_ONBOARDING);
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
    micRequestPending = true;
    try {
      const status = await invoke(CMD.REQUEST_MICROPHONE);
      micRequestPending = false;
      renderPermissions({ accessibility: accessibilityGranted, microphone: status });
    } catch (_) {
      micRequestPending = false;
      btn.disabled = false;
      btn.textContent = "Enable";
    }
  });

  $("#ob-finish").addEventListener("click", finish);
  updateFinish(); // gate immediately so it never flashes an enabled "Finish"

  // Retry a failed download. spawn_download is guarded backend-side, so a
  // double-tap is harmless; reset the row optimistically for instant feedback.
  $$("[data-retry]").forEach((b) =>
    b.addEventListener("click", () => {
      const id = b.dataset.retry;
      b.hidden = true;
      const row = $(`#${DL_EL[id]}`);
      if (row) {
        const fill = $("[data-fill]", row);
        fill.classList.remove("failed");
        fill.style.width = "0%";
        $("[data-pct]", row).textContent = "Starting…";
      }
      invoke?.(CMD.RETRY_DOWNLOAD, { id }).catch(() => {
        b.hidden = false;
      });
    })
  );

  // "Try it" push-to-talk. Pointer capture keeps the release event even if the
  // cursor drifts off the button mid-hold.
  const tryMic = $("#try-mic");
  if (tryMic) {
    tryMic.addEventListener("pointerdown", (e) => {
      e.preventDefault();
      tryMic.setPointerCapture?.(e.pointerId);
      tryStart();
    });
    tryMic.addEventListener("pointerup", (e) => {
      tryMic.releasePointerCapture?.(e.pointerId);
      tryStop();
    });
    tryMic.addEventListener("pointercancel", tryStop);
  }
  listen?.(EVENTS.TEST_DICTATION_RESULT, (e) => renderTryResult(e.payload));

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
