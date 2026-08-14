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

// Whether the Fn dictation tap is live. It installs at startup gated on
// Accessibility and — via fn_key::try_activate on the backend — re-installs live
// when the grant lands during onboarding. Until it's true, Fn can't be tested and
// finishing needs a relaunch to activate it.
let fnTapActive = false;

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
  // Arm the "Try it" test only while its step is showing, so a real Fn/chord
  // press elsewhere in onboarding still behaves normally. Refresh the card too.
  invoke?.(CMD.SET_ONBOARDING_TEST, { armed: step === TRY_STEP }).catch(() => {});
  if (step === TRY_STEP) updateTryCard();
}

// --- Permissions -------------------------------------------------------------

function setBadge(rowId, text, kind) {
  const badge = $(`#${rowId} [data-badge]`);
  if (!badge) return;
  badge.textContent = text;
  badge.className = `perm-badge${kind ? " " + kind : ""}`;
}

// A relaunch is only needed if Accessibility is granted but the Fn tap didn't
// come up live (the rare "born disabled" case). When install-on-grant works —
// the common path — the tap is already active and no relaunch is required.
function needsRelaunch() {
  return accessibilityGranted && !fnTapActive;
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
    fnTapActive = !!status.fn_tap_active;
    renderPermissions(status);
    // Seed the download bars for anything already on disk.
    if (status.whisper_ready) markDownloadDone(DOWNLOAD.WHISPER);
    if (status.llm_ready) markDownloadDone(DOWNLOAD.LLM);
    if (status.kokoro_ready) markDownloadDone(DOWNLOAD.KOKORO);
    // Reflect speech-model readiness on the Finish button (gated on Whisper).
    updateFinish();
    updateTryCard();
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
    updateTryCard(); // Whisper just landed — the "Try it" prompt can go live
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

// --- Try it (guided first success, real hotkey) ------------------------------

// "idle" (showing the prompt), "recording" (Fn held), or "transcribing". The
// backend drives recording/transcribing/done via the TEST_DICTATION_RESULT
// event; updateTryCard only touches the card while idle so it can't clobber a
// live "Listening…" message.
let tryPhase = "idle";

function setCard(headlineHtml, subText) {
  $("#try-headline").innerHTML = headlineHtml;
  $("#try-sub").textContent = subText;
}

// The idle prompt, reflecting what's actually possible right now: hold Fn (tap
// live + model ready), wait for the model, or finish-to-activate Fn.
function updateTryCard() {
  if (tryPhase !== "idle") return;
  const card = $("#try-card");
  if (!card) return;
  card.classList.remove("recording");
  if (invoke && !fnTapActive) {
    setCard("Fn turns on after setup", "Finish and Murmur restarts to activate it.");
  } else if (invoke && !whisper.ready) {
    setCard("Preparing the speech model…", "It downloads once, then runs offline.");
  } else {
    setCard("Hold <kbd>Fn</kbd> and speak", "Release when you're done.");
  }
}

function renderTryEvent({ phase, text, heard_audio }) {
  tryPhase = phase === "done" ? "idle" : phase;
  const card = $("#try-card");
  if (phase === "recording") {
    card.classList.add("recording");
    setCard("Listening…", "Keep talking — release when you're done.");
    $("#try-result").hidden = true;
    return;
  }
  if (phase === "transcribing") {
    card.classList.remove("recording");
    setCard("Transcribing…", "One moment.");
    return;
  }
  // done
  card.classList.remove("recording");
  const result = $("#try-result");
  const transcript = $("#try-transcript");
  const status = $("#try-status");
  result.hidden = false;
  result.classList.remove("ok", "warn");
  if (heard_audio && text) {
    transcript.textContent = `“${text}”`;
    status.textContent = "Heard you clearly — transcribed on-device. ✓";
    result.classList.add("ok");
  } else if (!heard_audio) {
    transcript.textContent = "";
    status.textContent =
      "We couldn't hear anything. Check your mic is connected and Murmur has Microphone access (go Back a step), then hold Fn to try again.";
    result.classList.add("warn");
  } else {
    transcript.textContent = "";
    status.textContent = "We didn't catch any words — hold Fn and try again, a little louder.";
    result.classList.add("warn");
  }
  updateTryCard(); // reset the prompt for another go
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

  // "Try it" runs off the real dictate key (Fn/chord). The backend routes an
  // armed test press into recording → transcribing → done phases here.
  listen?.(EVENTS.TEST_DICTATION_RESULT, (e) => renderTryEvent(e.payload));

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
