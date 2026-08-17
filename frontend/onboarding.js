// First-run onboarding. A small stepper that walks the user through
// permissions and the one-time model downloads, then relaunches (if needed) so
// the Fn tap picks up a freshly granted Accessibility permission.

import { EVENTS, CMD, DOWNLOAD } from "./constants.js";
import { bindRecorder } from "./recorder.js";
import { prettyShortcut } from "./shortcuts.js";

const invoke = window.__TAURI__?.core?.invoke;
const listen = window.__TAURI__?.event?.listen;

const STEPS = 6;
let step = 0;

// The current dictate trigger + read-aloud chord, mirrored from config so the
// cards can show the right keys and the pickers restore on a failed change.
let dictationTrigger = "Fn";
let ttsHotkey = "CmdOrCtrl+Shift+R";
let ttsRecorder = null;

// kbd label for each dictate trigger (matches Settings' badge map).
const TRIGGER_KBD = {
  Fn: "Fn",
  RightCtrl: "Right ⌃",
  RightAlt: "Right ⌥",
  RightCmd: "Right ⌘",
  Ctrl: "⌃",
  Alt: "⌥",
  Cmd: "⌘",
};

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

// Step 2 downloads models; step 3 tests dictation; step 4 tests read-aloud.
const DOWNLOAD_STEP = 2;
const TRY_STEP = 3;
const READ_STEP = 4;

// Every model must finish before leaving the download step — Continue stays
// blocked until then, so nobody lands in dictation/read-aloud on a half-fetched
// model.
function allModelsReady() {
  return dlDone[DOWNLOAD.WHISPER] && dlDone[DOWNLOAD.LLM] && dlDone[DOWNLOAD.KOKORO];
}

// The shared full-width CTA: its label + enabled state follow the current step.
// On the download step it stays disabled until every model is ready.
function updateCta() {
  const cur = stepEl(step);
  const cta = $("#ob-cta");
  if (!cur || !cta) return;
  if (invoke && cur.dataset.gate === "download" && !allModelsReady()) {
    cta.disabled = true;
    cta.textContent = "Downloading models…";
  } else {
    cta.disabled = false;
    cta.textContent = cur.dataset.cta;
  }
}

function goTo(n) {
  step = Math.max(0, Math.min(STEPS - 1, n));
  $$(".ob-step").forEach((s) => {
    s.hidden = Number(s.dataset.step) !== step;
  });
  $("#ob-progress").style.width = `${stepEl(step).dataset.prog}%`;
  $("#ob-back").hidden = step === 0;
  updateCta();
  // Start the Kokoro download when the user reaches the downloads step, so its
  // bar fills alongside Whisper/Qwen.
  if (step === DOWNLOAD_STEP) startNeural();
  // Arm the matching test only while its step is showing, so a real key press
  // elsewhere in onboarding still behaves normally.
  const testStep = step === TRY_STEP ? "dictation" : step === READ_STEP ? "read" : "none";
  invoke?.(CMD.SET_ONBOARDING_TEST, { step: testStep }).catch(() => {});
  if (step === TRY_STEP) {
    tryPhase = "idle"; // fresh placeholder each time the step is shown
    updateTryCard();
  }
  if (step === READ_STEP) readPrompt();
}

// --- Permissions -------------------------------------------------------------

// A row's right slot shows the green "Granted" badge once held, otherwise the
// accent action button — mirroring the reference (never both). Returns the btn.
function setPerm(rowId, granted, actionSel) {
  const badge = $(`#${rowId} [data-badge]`);
  const btn = $(`#${rowId} ${actionSel}`);
  if (badge) badge.hidden = !granted;
  if (btn) btn.hidden = granted;
  return btn;
}

// A relaunch is only needed if Accessibility is granted but the Fn tap didn't
// come up live (the rare "born disabled" case). When install-on-grant works —
// the common path — the tap is already active and no relaunch is required.
function needsRelaunch() {
  return accessibilityGranted && !fnTapActive;
}

function renderPermissions(status) {
  // Accessibility
  accessibilityGranted = !!status.accessibility;
  const axBtn = setPerm("perm-ax", accessibilityGranted, "[data-open-ax]");
  if (axBtn) axBtn.disabled = false;
  // Relaunch note (and the Finish-time relaunch) apply only to a grant made
  // *this session* — if Accessibility was already on at launch, nothing to do.
  $("#ax-relaunch-note").hidden = !needsRelaunch();

  // Microphone: 0 notDetermined, 1 restricted, 2 denied, 3 authorized
  // While a request is in flight the click handler owns the button (showing
  // "Waiting…"); don't let the status poll clobber it back to "Enable".
  if (micRequestPending) return;
  const mic = status.microphone;
  const micBtn = setPerm("perm-mic", mic === 3, "[data-mic-action]");
  if (mic !== 3 && micBtn) {
    micBtn.disabled = false;
    // Denied/restricted can only be flipped in System Settings; not-set prompts.
    const denied = mic === 2 || mic === 1;
    micBtn.textContent = denied ? "Open Settings" : "Enable";
    micBtn.dataset.mode = denied ? "settings" : "request";
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
    updateCta();
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

// Whisper's live progress, used only to word the "Try it" card ("Preparing the
// speech model…") until it's on disk. All three models gate the download step's
// Continue (see updateCta); this object just tracks Whisper for that message.
const whisper = { ready: false, failed: false, downloaded: 0, total: 0 };

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
    updateCta();
    updateTryCard(); // Whisper just landed — the "Try it" prompt can go live
  }
  const row = $(`#${DL_EL[id]}`);
  if (!row) return;
  const fill = $("[data-fill]", row);
  fill.style.width = "100%";
  fill.classList.add("done");
  fill.classList.remove("failed");
  setRetry(id, false);
  const pct = $("[data-pct]", row);
  pct.classList.add("done");
  pct.innerHTML =
    '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.6" stroke-linecap="round" stroke-linejoin="round"><path d="M20 6 9 17l-5-5" /></svg>Ready';
}

function renderDownload({ id, downloaded, total, failed }) {
  const row = $(`#${DL_EL[id]}`);
  if (!row) return;
  // Keep the Finish gate in sync with Whisper's live progress.
  if (id === DOWNLOAD.WHISPER) {
    whisper.downloaded = downloaded;
    whisper.total = total;
    whisper.failed = failed;
    updateCta();
  }
  const fill = $("[data-fill]", row);
  const pct = $("[data-pct]", row);
  pct.classList.remove("done"); // re-download: drop any prior "✓ Ready"
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
      pct.textContent = `${fmtMB(downloaded)} / ${fmtMB(total)}`;
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

// "idle" (placeholder), "recording" (key held), "transcribing", or "done"
// (transcript shown). The backend drives recording/transcribing/done via the
// TEST_DICTATION_RESULT event; updateTryCard only touches the box while idle so
// it can't clobber a live "Listening…" message or the shown transcript.
let tryPhase = "idle";

// Paint the record box: the main line, an optional status line under it, whether
// the caret blinks (input-like states) and which state class to apply.
function setCard(headline, status, { caret = false, cls = "" } = {}) {
  const card = $("#try-card");
  if (!card) return;
  card.classList.remove("recording", "ok", "warn");
  if (cls) card.classList.add(cls);
  $("#try-headline").textContent = headline;
  const cur = $("#try-cur");
  if (cur) cur.hidden = !caret;
  const st = $("#try-status");
  if (st) {
    st.textContent = status ?? "";
    st.hidden = !status;
  }
}

// The idle placeholder, reflecting what's actually possible right now: type-to-
// dictate (tap live + model ready), wait for the model, or finish-to-activate Fn.
function updateTryCard() {
  const keyEl = $("#try-key");
  if (keyEl) keyEl.textContent = TRIGGER_KBD[dictationTrigger] ?? "Fn";
  if (tryPhase !== "idle" || !$("#try-card")) return;
  if (invoke && !fnTapActive) {
    setCard("Fn turns on after setup — finish and Murmur restarts to activate it.", null);
  } else if (invoke && !whisper.ready) {
    setCard("Preparing the speech model… it downloads once, then runs offline.", null);
  } else {
    setCard("Say “This is my first dictation with Murmur”", null, { caret: true });
  }
}

function renderTryEvent({ phase, text, heard_audio }) {
  tryPhase = phase; // keep "done" so a status poll can't wipe the transcript
  if (phase === "recording") {
    setCard("Listening…", "Keep talking — release when you're done.", {
      caret: true,
      cls: "recording",
    });
    return;
  }
  if (phase === "transcribing") {
    setCard("Transcribing…", "One moment.");
    return;
  }
  // done — leave the result on screen; the next key press flips to "Listening…"
  if (heard_audio && text) {
    setCard(`“${text}”`, "Heard you clearly — transcribed on-device. ✓", { cls: "ok" });
  } else if (!heard_audio) {
    setCard(
      "We couldn't hear anything.",
      "Check your mic is connected and Murmur has Microphone access (go Back a step), then hold your key to try again.",
      { cls: "warn" },
    );
  } else {
    setCard("No words caught.", "Hold your key and try again, a little louder.", { cls: "warn" });
  }
}

// --- Try read-aloud ----------------------------------------------------------

// Sync the sub's key pill and drive the status line under the sample. With no
// headline the line is hidden (the reference shows nothing until you try it).
function readPrompt(headline, sub, { active = false } = {}) {
  const keyEl = $("#read-key");
  if (keyEl) keyEl.textContent = prettyShortcut(ttsHotkey);
  const card = $("#read-card");
  if (!card) return;
  card.classList.remove("recording");
  if (headline == null) {
    card.hidden = true;
    return;
  }
  card.hidden = false;
  $("#read-headline").textContent = headline;
  $("#read-sub").textContent = sub ?? "";
  if (active) card.classList.add("recording");
}

function renderReadEvent({ phase }) {
  if (phase === "speaking") {
    readPrompt("Playing…", "You should hear it now.", { active: true });
  } else if (phase === "unavailable") {
    readPrompt("Voice still downloading…", "The neural voice is finishing its one-time download.");
  } else if (phase === "select-first") {
    readPrompt("Highlight the sample text above first.", "Then press your read-aloud key.");
  } else {
    readPrompt("Heard it?", "Highlight and press again to replay.");
  }
}

// --- Config (dictate trigger + read-aloud key) -------------------------------

async function loadConfig() {
  if (!invoke) return;
  try {
    const c = await invoke(CMD.GET_CONFIG);
    dictationTrigger = c.dictation_trigger ?? "Fn";
    ttsHotkey = c.hotkey_tts ?? "CmdOrCtrl+Shift+R";
    const sel = $("#ob-dictation-trigger");
    if (sel) sel.value = dictationTrigger;
    ttsRecorder?.render();
    readPrompt();
    updateTryCard();
  } catch (_) {
    /* transient — cards fall back to defaults */
  }
}

// --- Finish ------------------------------------------------------------------

async function finish() {
  const btn = $("#ob-cta");
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
  goTo(0);

  // Shared full-width CTA advances (or finishes on the last step); shared Back.
  $("#ob-cta").addEventListener("click", () => {
    if (step >= STEPS - 1) finish();
    else goTo(step + 1);
  });
  $("#ob-back").addEventListener("click", () => goTo(step - 1));

  // The window is chromeless (no title bar / close button), so Esc is the close
  // hatch — except on the live test steps, where Esc cancels the in-flight
  // dictation/read test instead of dismissing onboarding.
  document.addEventListener("keydown", (e) => {
    if (e.key !== "Escape" || step === TRY_STEP || step === READ_STEP) return;
    invoke?.(CMD.CLOSE_ONBOARDING).catch(() => {});
  });

  // Report the highlighted sample so the read-aloud test can speak it. The
  // backend can't synthesize Cmd+C for our own window (the trigger's Shift is
  // still held → Cmd+Shift+C copies nothing), so it reads this instead.
  document.addEventListener("selectionchange", () => {
    if (step !== READ_STEP) return;
    const text = window.getSelection?.().toString() ?? "";
    invoke?.(CMD.REPORT_READ_SELECTION, { text }).catch(() => {});
  });

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
  listen?.(EVENTS.TEST_READ_RESULT, (e) => renderReadEvent(e.payload));

  // Change the dictate key live (read by the Fn tap). Updates the "Try it" card.
  const trigSel = $("#ob-dictation-trigger");
  trigSel?.addEventListener("change", async (e) => {
    const trigger = e.target.value;
    try {
      await invoke(CMD.SET_DICTATION_TRIGGER, { trigger });
      dictationTrigger = trigger;
      updateTryCard();
    } catch (_) {
      e.target.value = dictationTrigger;
    }
  });

  // Change the read-aloud key live (re-registers the chord), via the same
  // press-your-combo recorder the Settings window uses.
  const ttsBtn = $("#ob-tts-recorder");
  if (ttsBtn) {
    ttsRecorder = bindRecorder(ttsBtn, {
      getCurrent: () => ttsHotkey,
      onCapture: async (shortcut) => {
        await invoke(CMD.SET_HOTKEY, { action: "tts_toggle", shortcut });
        ttsHotkey = shortcut;
        readPrompt();
      },
      // Suspend chords while capturing — otherwise pressing the current
      // read-aloud key just fires read-aloud instead of re-recording it.
      onOpen: () => invoke?.(CMD.SUSPEND_SHORTCUTS).catch(() => {}),
      onClose: () => invoke?.(CMD.RESUME_SHORTCUTS).catch(() => {}),
    });
  }
  loadConfig();

  // Model-download progress from the backend's prefetch.
  listen?.(EVENTS.MODEL_DOWNLOAD, (e) => renderDownload(e.payload));

  // Permissions change in System Settings (outside the app), so poll.
  refreshStatus();
  setInterval(refreshStatus, 1500);

  // Dev screenshot tooling: a debug build launched with MURMUR_UI_STEP jumps
  // straight to that step via window.__LAUNCH_STEP (set by an initialization
  // script — see show_onboarding_window), so scripts/ui-shot.mjs can capture
  // each step. Unset in normal use.
  if (window.__LAUNCH_STEP != null) goTo(Number(window.__LAUNCH_STEP));
}

if (document.readyState === "loading") {
  window.addEventListener("DOMContentLoaded", init);
} else {
  init();
}
