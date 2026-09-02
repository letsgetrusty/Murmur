// Settings/history window. Talks to the Rust backend over Tauri IPC commands
// (the overlay is event-driven; this window is request/response).

import { EVENTS, CMD, TABS } from "./constants.js";
import { bindRecorder, cancelActiveRecorder } from "./recorder.js";
import { prettyShortcut } from "./shortcuts.js";

const invoke = window.__TAURI__?.core?.invoke;

// The full config object, kept so Save round-trips fields this UI doesn't edit.
let currentConfig = null;

const el = (id) => document.getElementById(id);
const setText = (id, text) => {
  const e = el(id);
  if (e) e.textContent = text;
};

// Short glyphs for the read-only "trigger" references shown on Dictation /
// Read-aloud / Home (the real bindings are edited on Shortcuts). TRIGGER_BADGE
// (defined below) maps the hold-to-talk trigger; these map the refine modifier.
const MOD_LABEL = { Ctrl: "⌃", Shift: "⇧", Alt: "⌥", Cmd: "⌘" };

// Repaint every read-only trigger chip from the live config.
function syncTriggerRefs() {
  const trig = TRIGGER_BADGE[currentConfig?.dictation_trigger ?? "Fn"] ?? "Fn";
  const mod = MOD_LABEL[currentConfig?.refine_modifier ?? "Ctrl"] ?? "⌃";
  const read = prettyShortcut(currentConfig?.hotkey_tts ?? "CmdOrCtrl+Shift+R");
  setText("dict-trigger-kbd", trig);
  setText("refine-trigger-kbd", trig);
  setText("refine-mod-kbd", mod);
  setText("read-trigger-kbd", read);
  setText("gs-dictate-kbd", trig);
  setText("gs-read-kbd", read);
}

// --- Settings tab: config load + engine switches -----------------------------

function setStatus(text, kind = "") {
  const s = el("status");
  s.textContent = text;
  s.className = `status ${kind}`;
}

// A small modal (confirm or info). Resolves true on OK/Enter, false on
// Cancel/Esc/backdrop. Pass cancelLabel=null for a plain info dialog. Used
// instead of window.confirm/alert, which Tauri's webview doesn't present.
function showModal({ message, okLabel = "OK", cancelLabel = null, danger = false }) {
  return new Promise((resolve) => {
    const backdrop = el("modal");
    const ok = el("modal-ok");
    const cancel = el("modal-cancel");
    el("modal-msg").textContent = message;
    ok.textContent = okLabel;
    cancel.textContent = cancelLabel ?? "Cancel";
    cancel.hidden = cancelLabel === null;
    backdrop.querySelector(".modal").classList.toggle("danger", danger);
    backdrop.hidden = false;
    const done = (result) => {
      backdrop.hidden = true;
      ok.removeEventListener("click", onOk);
      cancel.removeEventListener("click", onCancel);
      backdrop.removeEventListener("click", onBackdrop);
      document.removeEventListener("keydown", onKey, true);
      resolve(result);
    };
    const onOk = () => done(true);
    const onCancel = () => done(false);
    const onBackdrop = (e) => {
      if (e.target === backdrop) done(false);
    };
    const onKey = (e) => {
      if (e.key === "Escape") {
        e.preventDefault();
        e.stopPropagation();
        done(false);
      } else if (e.key === "Enter") {
        e.preventDefault();
        done(true);
      }
    };
    ok.addEventListener("click", onOk);
    cancel.addEventListener("click", onCancel);
    backdrop.addEventListener("click", onBackdrop);
    document.addEventListener("keydown", onKey, true);
    ok.focus();
  });
}

// Auto-save the refinement prompt: debounced while typing, saved immediately on
// blur. No Save button — settings apply as you change them.
let refineSaveTimer = null;
function queueRefineSave() {
  clearTimeout(refineSaveTimer);
  refineSaveTimer = setTimeout(saveRefinePrompt, 600);
}
async function saveRefinePrompt() {
  clearTimeout(refineSaveTimer);
  if (!invoke || !currentConfig) return;
  const value = el("refine-prompt").value;
  if (value === (currentConfig.refine_prompt ?? "")) return; // no change
  const next = { ...currentConfig, refine_prompt: value };
  setStatus("Saving…");
  try {
    await invoke(CMD.SAVE_CONFIG, { config: next });
    currentConfig = next;
    setStatus("Saved ✓", "ok");
    setTimeout(() => setStatus(""), 1500);
  } catch (e) {
    setStatus(`Save failed: ${e}`, "error");
  }
}

async function loadConfig() {
  if (!invoke) {
    setStatus("IPC unavailable", "error");
    return;
  }
  try {
    currentConfig = await invoke(CMD.GET_CONFIG);
    el("refine-prompt").value = currentConfig.refine_prompt ?? "";
    el("stt-model").value = currentConfig.stt_model ?? "small.en";
    el("llm-model").value = currentConfig.llm_model ?? "Qwen3-1.7B-Q4_K_M";
    el("tts-provider").value = currentConfig.tts_provider ?? "native";
    el("overlay-position").value = currentConfig.overlay_position ?? "bottom-center";
    syncTriggerRefs();
  } catch (e) {
    setStatus(`Load failed: ${e}`, "error");
  }
}

// Engine (STT model / TTS backend) selections save immediately and prompt a
// relaunch, since the backends are built once at startup — a plain config save
// wouldn't swap them.
async function saveEngines() {
  if (!invoke || !currentConfig) return;
  const next = {
    ...currentConfig,
    stt_model: el("stt-model").value,
    llm_model: el("llm-model").value,
    tts_provider: el("tts-provider").value,
  };
  try {
    await invoke(CMD.SAVE_CONFIG, { config: next });
    currentConfig = next;
    el("engine-relaunch").hidden = false;
  } catch (e) {
    setStatus(`Save failed: ${e}`, "error");
  }
}

// --- Audio (apply immediately) ------------------------------------------------

// Read-aloud speed is a segmented control (1× / 1.5× / 2×) — a small fixed set,
// so it reads better than a dropdown and matches the design reference.
function highlightSpeed(current) {
  const seg = el("speed-seg");
  if (!seg) return;
  for (const b of seg.querySelectorAll("button")) {
    b.classList.toggle("on", Math.abs(parseFloat(b.dataset.speed) - current) < 1e-6);
  }
}
function renderSpeedSeg(speeds, current) {
  const seg = el("speed-seg");
  if (!seg) return;
  seg.innerHTML = "";
  for (const s of speeds) {
    const b = document.createElement("button");
    b.type = "button";
    b.textContent = `${s}×`;
    b.dataset.speed = String(s);
    b.addEventListener("click", () => {
      invoke?.(CMD.SET_SPEED, { speed: s });
      if (currentConfig) currentConfig.tts_speed = s;
      highlightSpeed(s);
    });
    seg.appendChild(b);
  }
  highlightSpeed(current);
}

function addOption(select, value, label) {
  const o = document.createElement("option");
  o.value = value;
  o.textContent = label;
  select.appendChild(o);
}

// The mic is a custom button + menu (see initMicDropdown) — a native <select>
// leaves a big gap between the label and its chevron in the top bar.
const MIC_CHECK =
  '<svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.4" stroke-linecap="round" stroke-linejoin="round"><path d="M20 6 9 17l-5-5"/></svg>';
let micList = [];

// (Re)build the microphone menu from the current input-device list.
function fillMics(mics) {
  micList = mics;
  const menu = el("micMenu");
  const label = el("micLabel");
  if (!menu || !label) return;
  const current = currentConfig?.mic_name ?? "";
  const present = mics.includes(current);
  menu.querySelectorAll(".mic-opt").forEach((o) => o.remove());

  const add = (value, text) => {
    const b = document.createElement("button");
    b.type = "button";
    b.className = "mic-opt";
    b.dataset.value = value;
    const name = document.createElement("span");
    name.textContent = text;
    const ck = document.createElement("span");
    ck.className = "ck";
    ck.innerHTML = MIC_CHECK;
    b.append(name, ck);
    // "on" is the saved device if present, else System default.
    b.classList.toggle("on", value === "" ? !present : value === current);
    b.addEventListener("click", () => selectMic(value, text));
    menu.appendChild(b);
  };
  add("", "System default");
  for (const m of mics) add(m, m);
  label.textContent = present ? current : "System default";
}

function selectMic(value, text) {
  invoke?.(CMD.SET_MIC, { name: value === "" ? null : value }).catch(() => {});
  if (currentConfig) currentConfig.mic_name = value;
  el("micLabel").textContent = text;
  for (const o of el("micMenu").querySelectorAll(".mic-opt")) {
    o.classList.toggle("on", o.dataset.value === value);
  }
  closeMicMenu();
}

function closeMicMenu() {
  el("micMenu")?.setAttribute("hidden", "");
  el("micBtn")?.setAttribute("aria-expanded", "false");
}

function initMicDropdown() {
  const btn = el("micBtn");
  const menu = el("micMenu");
  if (!btn || !menu) return;
  btn.addEventListener("click", (e) => {
    e.stopPropagation();
    const open = menu.hidden;
    menu.hidden = !open;
    btn.setAttribute("aria-expanded", String(open));
  });
  document.addEventListener("click", closeMicMenu);
}

// Re-fetch just the input-device list. Called when the window is shown/refocused
// so a mic plugged in after launch appears without a relaunch.
async function refreshMics() {
  if (!invoke || !currentConfig) return;
  try {
    const opts = await invoke(CMD.GET_OPTIONS);
    fillMics(opts.mics);
  } catch (e) {
    /* transient — next focus/open retries */
  }
}

async function loadOptions() {
  if (!invoke || !currentConfig) return;
  let opts;
  try {
    opts = await invoke(CMD.GET_OPTIONS);
  } catch (e) {
    return;
  }

  renderSpeedSeg(opts.speeds, currentConfig.tts_speed);

  const voice = el("voice");
  voice.innerHTML = "";
  for (const v of opts.voices) addOption(voice, v.id, v.name);
  voice.value = currentConfig.tts_voice_id;

  fillMics(opts.mics);

  voice.addEventListener("change", async (e) => {
    await invoke(CMD.SET_VOICE, { voiceId: e.target.value });
    // Preview the new voice: "Hey, my name is <Name>!". The option label is the
    // friendly name (e.g. "Heart (US female)"); take the part before " (".
    const label = e.target.selectedOptions[0]?.textContent ?? "";
    const name = label.split("(")[0].trim();
    invoke(CMD.PREVIEW_VOICE, { name });
  });
  el("overlay-position").addEventListener("change", (e) => {
    const position = e.target.value;
    invoke(CMD.SET_OVERLAY_POSITION, { position }); // saves + flashes a preview
    if (currentConfig) currentConfig.overlay_position = position;
  });

  // Speed/voice/mic can also change from the tray or a hotkey (e.g. Cmd+Ctrl+S
  // cycles speed) while this window is open — re-sync those controls when they do.
  window.__TAURI__?.event?.listen?.(EVENTS.CONFIG_CHANGED, async () => {
    if (!invoke || !currentConfig) return;
    try {
      const cfg = await invoke(CMD.GET_CONFIG);
      currentConfig.tts_speed = cfg.tts_speed;
      currentConfig.tts_voice_id = cfg.tts_voice_id;
      currentConfig.mic_name = cfg.mic_name;
      // A tray/hotkey rebind can also move the read-aloud chord or dictate
      // trigger — keep the read-only trigger chips in sync too.
      currentConfig.hotkey_tts = cfg.hotkey_tts;
      currentConfig.dictation_trigger = cfg.dictation_trigger;
      currentConfig.refine_modifier = cfg.refine_modifier;
      highlightSpeed(cfg.tts_speed);
      el("voice").value = cfg.tts_voice_id;
      fillMics(micList); // re-render the mic menu label/highlight
      syncTriggerRefs();
    } catch (_) {
      /* transient */
    }
  });
}

// --- Keyboard shortcuts -------------------------------------------------------

const HOTKEY_FIELD = {
  dictate: "hotkey_dictate",
  tts_toggle: "hotkey_tts",
  tts_speed: "hotkey_tts_speed",
};

// Short label for the "Dictate & refine" combo's leading key — mirrors the
// chosen hold-to-talk trigger so the refine hint stays accurate.
const TRIGGER_BADGE = {
  Fn: "Fn",
  RightCtrl: "Right ⌃",
  RightAlt: "Right ⌥",
  RightCmd: "Right ⌘",
  Ctrl: "⌃",
  Alt: "⌥",
  Cmd: "⌘",
};
function syncTriggerBadge() {
  const badge = el("refine-trigger-badge");
  if (badge) badge.textContent = TRIGGER_BADGE[currentConfig?.dictation_trigger ?? "Fn"] ?? "Fn";
}

// The recorder relies on the control itself for confirmation — the button
// label (or dropdown value) updates to the new binding — so success and cancel
// need no message. Only errors get feedback, shown inline on the active control
// (#hotkey-status stays static help and is never overwritten).
function flashInvalid(elm) {
  elm.classList.add("invalid");
  setTimeout(() => elm.classList.remove("invalid"), 1800);
}

// One shared recorder per `.recorder` button; `renderHotkeys` repaints them all
// (after a Reset). Capture/validation lives in recorder.js.
const recorders = {};

function renderHotkeys() {
  for (const h of Object.values(recorders)) h.render();
}

function initHotkeys() {
  for (const btn of document.querySelectorAll(".recorder")) {
    const action = btn.dataset.action;
    recorders[action] = bindRecorder(btn, {
      getCurrent: () => currentConfig[HOTKEY_FIELD[action]],
      onCapture: async (shortcut) => {
        await invoke(CMD.SET_HOTKEY, { action, shortcut });
        currentConfig[HOTKEY_FIELD[action]] = shortcut; // persist for restore
        syncTriggerRefs(); // read-aloud chip reflects a tts_toggle rebind
      },
      // Suspend chords while capturing so a bound combo reaches the recorder
      // instead of firing; resume once the new binding is registered.
      onOpen: () => invoke?.(CMD.SUSPEND_SHORTCUTS).catch(() => {}),
      onClose: () => invoke?.(CMD.RESUME_SHORTCUTS).catch(() => {}),
    });
  }

  // Hold-to-talk trigger: Fn, a right-side modifier (a dedicated key for
  // keyboards without Fn), or a plain modifier. Applies live (read by the tap).
  const dt = el("dictation-trigger");
  if (dt) {
    dt.value = currentConfig.dictation_trigger ?? "Fn";
    syncTriggerBadge();
    dt.addEventListener("change", async (e) => {
      const trigger = e.target.value;
      try {
        await invoke(CMD.SET_DICTATION_TRIGGER, { trigger });
        currentConfig.dictation_trigger = trigger; // selected value confirms it
        syncTriggerBadge();
        syncTriggerRefs();
      } catch (err) {
        dt.value = currentConfig.dictation_trigger ?? "Fn";
        flashInvalid(dt);
      }
    });
  }

  // Refined dictation: the trigger + a configurable modifier.
  const rm = el("refine-modifier");
  if (rm) {
    rm.value = currentConfig.refine_modifier ?? "Ctrl";
    rm.addEventListener("change", async (e) => {
      const modifier = e.target.value;
      try {
        await invoke(CMD.SET_REFINE_MODIFIER, { modifier });
        currentConfig.refine_modifier = modifier; // selected value confirms it
        syncTriggerRefs();
      } catch (err) {
        rm.value = currentConfig.refine_modifier ?? "Ctrl";
        flashInvalid(rm);
      }
    });
  }

  // Reset every binding to its default. The chords re-register live; the trigger
  // and refine modifier apply live via the Fn tap. The backend returns the fresh
  // config so we can repaint all the controls at once.
  const reset = el("reset-hotkeys");
  if (reset) {
    reset.addEventListener("click", async () => {
      const ok = await showModal({
        message: "Reset all key bindings to their defaults?",
        okLabel: "Reset",
        cancelLabel: "Cancel",
      });
      if (!ok) return;
      cancelActiveRecorder();
      try {
        currentConfig = await invoke(CMD.RESET_HOTKEYS);
        renderHotkeys();
        if (dt) dt.value = currentConfig.dictation_trigger ?? "Fn";
        if (rm) rm.value = currentConfig.refine_modifier ?? "Ctrl";
        syncTriggerBadge();
        syncTriggerRefs();
        setStatus("Key bindings reset to defaults ✓", "ok");
        setTimeout(() => setStatus(""), 1500);
      } catch (err) {
        setStatus(`Reset failed: ${err}`, "error");
      }
    });
  }
}

// --- Usage --------------------------------------------------------------------

let localUsage = null; // from get_usage (dictation/refine/read-aloud counts)

async function loadUsage() {
  if (!invoke) return;
  try {
    localUsage = await invoke(CMD.GET_USAGE);
  } catch (e) {
    /* ignore */
  }
  renderInsights();
}

function initUsage() {
  // Insights auto-loads on tab open and live-updates from the backend's "usage"
  // event (fired after every dictation / read-aloud), so no manual refresh.
  window.__TAURI__?.event?.listen?.(EVENTS.USAGE, (e) => {
    localUsage = e.payload;
    renderInsights();
  });
}

// --- Home stats ---------------------------------------------------------------

let insStats = null; // last history_stats { total, refined, words, first_ts, days[] }

// Human "time saved" — a coarse estimate vs typing at ~40 wpm, floored at zero.
function fmtSaved(sec) {
  sec = Math.round(sec || 0);
  if (sec < 60) return `${sec}s`;
  const min = sec / 60;
  if (min < 60) return `${Math.round(min)} min`;
  return `${(min / 60).toFixed(1)} hrs`;
}

function renderInsights() {
  const u = localUsage || {};
  const s = insStats || { total: 0, refined: 0, words: 0, days: [] };

  const words = s.words || 0;
  const speakSec = u.stt_seconds || 0;
  const wpm = speakSec > 0 ? Math.round(words / (speakSec / 60)) : 0;
  // Time saved vs typing the same words at ~40 wpm.
  const savedSec = Math.max(0, (words / 40) * 60 - speakSec);

  setText("ins-wpm", wpm.toLocaleString());
  setText("ins-words", words.toLocaleString());
  setText("ins-dictations", (u.stt_count || 0).toLocaleString());
  setText("ins-saved", fmtSaved(savedSec));
}

async function loadInsights() {
  if (!invoke) return;
  try {
    insStats = await invoke(CMD.HISTORY_STATS);
  } catch (_) {
    insStats = null;
  }
  await loadUsage(); // refreshes the local usage counts, then re-renders
  renderInsights();
}

// --- History ------------------------------------------------------------------

let historyQuery = "";

function fmtTime(ts) {
  const d = new Date(ts * 1000);
  const diff = (Date.now() - d.getTime()) / 1000;
  if (diff < 60) return "just now";
  if (diff < 3600) return `${Math.floor(diff / 60)}m ago`;
  if (diff < 86400) return `${Math.floor(diff / 3600)}h ago`;
  return (
    d.toLocaleDateString(undefined, { month: "short", day: "numeric" }) +
    ", " +
    d.toLocaleTimeString(undefined, { hour: "numeric", minute: "2-digit" })
  );
}

const TRASH_SVG =
  '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M3 6h18M8 6V4a1 1 0 0 1 1-1h6a1 1 0 0 1 1 1v2m3 0v14a1 1 0 0 1-1 1H6a1 1 0 0 1-1-1V6M10 11v6M14 11v6"/></svg>';

// A history entry: the transcript, then a dim meta line (time · words · refined),
// with Copy + a trash-icon delete on the right — matching the design reference.
function renderHistoryRow(e) {
  const text = e.refined ?? e.raw;
  const words = text.trim().split(/\s+/).filter(Boolean).length;
  const meta = `${fmtTime(e.ts)} · ${words} word${words === 1 ? "" : "s"}${
    e.refined ? " · refined" : ""
  }`;

  const row = document.createElement("div");
  row.className = "row";

  const lbl = document.createElement("div");
  lbl.className = "lbl";
  const small = document.createElement("small");
  small.textContent = meta;
  lbl.append(document.createTextNode(text), small);

  const actions = document.createElement("span");
  actions.className = "hactions";
  const copy = document.createElement("button");
  copy.className = "btn";
  copy.textContent = "Copy";
  copy.addEventListener("click", async () => {
    try {
      await invoke(CMD.COPY_TEXT, { text });
      copy.textContent = "Copied";
      setTimeout(() => (copy.textContent = "Copy"), 1200);
    } catch (_) {}
  });
  const del = document.createElement("button");
  del.className = "btn ibtn";
  del.title = "Delete";
  del.innerHTML = TRASH_SVG;
  del.addEventListener("click", async () => {
    try {
      await invoke(CMD.DELETE_HISTORY, { id: e.id });
      row.remove();
      if (!el("history-list").children.length) el("history-empty").hidden = false;
    } catch (_) {}
  });
  actions.append(copy, del);

  row.append(lbl, actions);
  return row;
}

async function loadHistory() {
  if (!invoke) return;
  try {
    const entries = await invoke(CMD.LIST_HISTORY, {
      query: historyQuery,
      limit: 200,
      offset: 0,
    });
    const list = el("history-list");
    list.innerHTML = "";
    el("history-empty").hidden = entries.length > 0;
    for (const e of entries) list.appendChild(renderHistoryRow(e));
  } catch (_) {}
}

function initHistory() {
  let t = null;
  el("history-search").addEventListener("input", (e) => {
    historyQuery = e.target.value;
    clearTimeout(t);
    t = setTimeout(loadHistory, 150);
  });

  el("history-clear").addEventListener("click", async () => {
    const ok = await showModal({
      message: "Delete all dictation history? This can't be undone.",
      okLabel: "Delete all",
      cancelLabel: "Cancel",
      danger: true,
    });
    if (!ok) return;
    try {
      await invoke(CMD.CLEAR_HISTORY);
      loadHistory();
    } catch (_) {}
  });

  // Refresh live when a new dictation lands and we're on the relevant tab.
  window.__TAURI__?.event?.listen?.(EVENTS.HISTORY, () => {
    if (document.getElementById(`tab-${TABS.HISTORY}`)?.classList.contains("active")) loadHistory();
    if (document.getElementById(`tab-${TABS.HOME}`)?.classList.contains("active")) loadInsights();
  });
}

// --- Support ------------------------------------------------------------------

const REPO_URL = "https://github.com/letsgetrusty/Murmur";

async function loadSupport() {
  if (!invoke) return;
  try {
    const version = await invoke(CMD.APP_VERSION);
    setText("app-version", `v${version}`);
    setText("brand-ver", `v${version}`);
  } catch (_) {
    setText("app-version", "unknown");
  }
}

function initSupport() {
  const open = (url) => invoke?.(CMD.OPEN_URL, { url }).catch(() => {});
  el("report-issue")?.addEventListener("click", reportBug);
  el("view-github")?.addEventListener("click", () => open(REPO_URL));
}

// Gather diagnostics, copy the full report to the clipboard, and open a
// prefilled GitHub issue. The command does the work; we just confirm.
async function reportBug() {
  if (!invoke) return;
  try {
    const summary = await invoke(CMD.REPORT_BUG);
    showModal({
      message:
        "A prefilled GitHub issue just opened in your browser. Your diagnostics and recent log were copied to the clipboard — paste them into the issue body.\n\n" +
        (summary || ""),
    });
  } catch (e) {
    showModal({ message: `Couldn't gather diagnostics: ${e}` });
  }
}

// --- Sound (dictation start/stop cue) ----------------------------------------

function paintSoundCue() {
  const t = el("sound-cue");
  if (!t) return;
  const on = currentConfig?.dictation_sound ?? true;
  t.classList.toggle("on", on);
  t.setAttribute("aria-checked", String(on));
}

function initSound() {
  const t = el("sound-cue");
  if (!t) return;
  paintSoundCue();
  t.addEventListener("click", async () => {
    if (!invoke || !currentConfig) return;
    const next = { ...currentConfig, dictation_sound: !currentConfig.dictation_sound };
    try {
      await invoke(CMD.SAVE_CONFIG, { config: next });
      currentConfig = next;
      paintSoundCue();
    } catch (e) {
      setStatus(`Save failed: ${e}`, "error");
    }
  });
}

// Read-aloud: "fall back to the clipboard when nothing's selected" (default on).
function paintClipFallback() {
  const t = el("clip-fallback");
  if (!t) return;
  const on = currentConfig?.tts_clipboard_fallback ?? true;
  t.classList.toggle("on", on);
  t.setAttribute("aria-checked", String(on));
}
function initClipFallback() {
  const t = el("clip-fallback");
  if (!t) return;
  paintClipFallback();
  t.addEventListener("click", async () => {
    if (!invoke || !currentConfig) return;
    const on = !(currentConfig.tts_clipboard_fallback ?? true);
    try {
      await invoke(CMD.SAVE_CONFIG, { config: { ...currentConfig, tts_clipboard_fallback: on } });
      currentConfig = { ...currentConfig, tts_clipboard_fallback: on };
      paintClipFallback();
    } catch (e) {
      setStatus(`Save failed: ${e}`, "error");
    }
  });
}

// --- Tabs & init --------------------------------------------------------------

function switchTab(name) {
  for (const b of document.querySelectorAll(".nav-item")) {
    b.classList.toggle("active", b.dataset.p === name);
  }
  for (const t of document.querySelectorAll(".pane")) {
    t.classList.toggle("active", t.id === `tab-${name}`);
  }
  document.querySelector(".content").scrollTop = 0;
  if (name === TABS.HISTORY) loadHistory();
  if (name === TABS.HOME) loadInsights();
}

// --- UI zoom (Cmd +/-/0) ------------------------------------------------------
// WKWebView ignores Tauri's native zoom hotkeys, so scale the page with CSS
// `zoom` and persist the level (survives the window being recreated on reopen).
const ZOOM_KEY = "ui-zoom";
let uiZoom = parseFloat(localStorage.getItem(ZOOM_KEY)) || 1;

function applyZoom() {
  uiZoom = Math.round(Math.min(Math.max(uiZoom, 0.6), 2.5) * 100) / 100;
  // Zoom only the scrollable content — not the whole document — so the shell
  // (sidebar + its footer, top bar) stays fixed and the window never scrolls.
  const content = document.querySelector(".content");
  if (content) content.style.zoom = String(uiZoom);
  // Clear any stale document-level zoom from older builds.
  document.documentElement.style.zoom = "";
  localStorage.setItem(ZOOM_KEY, String(uiZoom));
}

function initZoom() {
  applyZoom(); // re-apply the saved level on load
  window.addEventListener("keydown", (e) => {
    if (!e.metaKey) return;
    if (e.key === "=" || e.key === "+") {
      e.preventDefault();
      uiZoom += 0.1;
    } else if (e.key === "-" || e.key === "_") {
      e.preventDefault();
      uiZoom -= 0.1;
    } else if (e.key === "0") {
      e.preventDefault();
      uiZoom = 1;
    } else {
      return;
    }
    applyZoom();
  });
}

// --- Auto-update --------------------------------------------------------------

// Escape HTML, then re-apply the only markup we render from release notes:
// **bold**. Notes come from our own signed release, but escaping first keeps a
// stray `<` in a commit subject from breaking the layout.
function notesInline(text) {
  const escaped = text
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;");
  return escaped.replace(/\*\*([^*]+)\*\*|__([^_]+)__/g, (_, a, b) => `<b>${a || b}</b>`);
}

// Strip one changelog bullet down to its human-readable subject: drop the
// GitHub auto-notes "by @user in <url>" tail, collapse `[text](url)` links to
// their text, and shed a leading conventional-commit type (`feat:`, `fix(x):`).
function cleanNoteItem(line) {
  return line
    .replace(/\s+by\s+@[\w-]+\s+in\s+\S+$/i, "")
    .replace(/\[([^\]]+)\]\((?:[^)]+)\)/g, "$1")
    .replace(/^(?:feat|fix|perf|refactor|docs|chore|build|ci|test|style)(?:\([^)]*\))?!?:\s*/i, "")
    .trim();
}

// Parse markdown release notes into ordered { label, items } sections. Headings
// (`## New`) open a section; `-`/`*` lines are its bullets. Prose with no
// structure (e.g. a one-line release blurb) falls under a single unlabeled
// section so it still renders. The GitHub "Full Changelog" line is dropped — we
// surface the diff via our own "View the exact diff" link.
function parseUpdateNotes(md) {
  const sections = [];
  let current = null;
  const ensure = (label) => {
    if (!current || current.label !== label) {
      current = { label, items: [] };
      sections.push(current);
    }
    return current;
  };
  for (const raw of (md || "").split(/\r?\n/)) {
    const line = raw.trim();
    if (!line || line === "---" || /^\*\*full changelog\*\*/i.test(line)) continue;
    const heading = line.match(/^#{1,6}\s+(.*)$/);
    if (heading) {
      current = { label: heading[1].replace(/[:*]+$/, "").trim(), items: [] };
      sections.push(current);
      continue;
    }
    const bullet = line.match(/^[-*]\s+(.*)$/);
    const item = cleanNoteItem(bullet ? bullet[1] : line);
    if (item) ensure(current?.label ?? "").items.push(item);
  }
  return sections.filter((s) => s.items.length);
}

function renderUpdateNotes(md) {
  const host = el("update-notes");
  host.textContent = "";
  const sections = parseUpdateNotes(md);
  if (!sections.length) {
    const p = document.createElement("p");
    p.className = "update-empty";
    p.textContent = "No release notes were provided — see the exact diff below.";
    host.appendChild(p);
    return;
  }
  for (const section of sections) {
    const cat = document.createElement("div");
    cat.className = "update-cat";
    if (section.label) {
      const k = document.createElement("div");
      k.className = "update-cat-k";
      k.textContent = section.label;
      cat.appendChild(k);
    }
    const ul = document.createElement("ul");
    ul.className = "update-items";
    for (const item of section.items) {
      const li = document.createElement("li");
      li.innerHTML = notesInline(item);
      ul.appendChild(li);
    }
    cat.appendChild(ul);
    host.appendChild(cat);
  }
}

// A newer version has been downloaded + verified in the background and is
// staged — the user just needs to restart to apply it. `info` carries the new
// version, the one it replaces, and the release notes.
function showUpdateStaged(info) {
  const { version, currentVersion, notes } = info || {};
  if (!version) return;
  el("update-text").innerHTML = `<b>Murmur ${version}</b> is ready to install`;
  const btn = el("update-install");
  btn.hidden = false;
  btn.disabled = false;
  btn.textContent = "Install & Restart";

  el("update-notes-title").textContent = `What's new in ${version}`;
  el("update-range").innerHTML = currentVersion
    ? `<b>v${currentVersion}</b> → <b>v${version}</b>`
    : "";
  renderUpdateNotes(notes);
  if (currentVersion) {
    el("update-diff").dataset.url = `${REPO_URL}/compare/v${currentVersion}...v${version}`;
  }
  el("update-banner").hidden = false;
}

function showUpToDate() {
  showModal({ message: "You're on the latest version of Murmur." });
}

async function installUpdate() {
  const btn = el("update-install");
  btn.disabled = true;
  btn.textContent = "Restarting…";
  try {
    // Installs the pre-downloaded update and relaunches — never resolves.
    await invoke(CMD.INSTALL_STAGED_UPDATE);
  } catch (e) {
    el("update-text").textContent = `Update failed: ${e}`;
    btn.disabled = false;
    btn.textContent = "Retry";
  }
}

function toggleUpdateDetails() {
  const btn = el("update-expand");
  const details = el("update-details");
  const open = details.hidden;
  details.hidden = !open;
  btn.setAttribute("aria-expanded", String(open));
  btn.setAttribute("aria-label", open ? "Hide what changed" : "Show what changed");
}

function initUpdate() {
  el("update-install").addEventListener("click", installUpdate);
  el("update-expand").addEventListener("click", toggleUpdateDetails);
  // Open the diff in the default browser, not inside the WKWebview.
  el("update-diff").addEventListener("click", (e) => {
    e.preventDefault();
    const url = e.currentTarget.dataset.url;
    if (url) invoke?.(CMD.OPEN_URL, { url }).catch(() => {});
  });
  const listen = window.__TAURI__?.event?.listen;
  listen?.(EVENTS.UPDATE_STAGED, (e) => showUpdateStaged(e.payload));
  listen?.(EVENTS.UPDATE_NONE, () => showUpToDate());
  // If an update was already staged before this window opened, show it.
  invoke?.(CMD.PENDING_UPDATE_VERSION).then((info) => {
    if (info) showUpdateStaged(info);
  });
}

async function init() {
  initZoom();
  for (const b of document.querySelectorAll(".nav-item")) {
    b.addEventListener("click", () => switchTab(b.dataset.p));
  }
  el("nav-toggle")?.addEventListener("click", () => {
    document.querySelector(".win").classList.toggle("nav-collapsed");
  });
  initMicDropdown();
  // In-content links that jump to a tab (Home's "Customize shortcuts", the
  // "Rebind in Shortcuts" hints on Dictation/Read-aloud).
  for (const e of document.querySelectorAll("[data-goto]")) {
    e.addEventListener("click", () => switchTab(e.dataset.goto));
  }
  for (const id of ["stt-model", "llm-model", "tts-provider"]) {
    el(id).addEventListener("change", saveEngines);
  }
  el("engine-relaunch-btn").addEventListener("click", () => invoke?.(CMD.RELAUNCH_APP));
  // Refinement prompt auto-saves (debounced while typing, flushed on blur).
  el("refine-prompt").addEventListener("input", queueRefineSave);
  el("refine-prompt").addEventListener("blur", saveRefinePrompt);

  await loadConfig();
  await loadOptions();
  initHotkeys();
  initSound();
  initClipFallback();
  initUsage();
  initHistory();
  initUpdate();
  initSupport();
  loadSupport();

  // The settings window is reused across opens (shown/focused, not recreated),
  // so init() runs once. Re-enumerate input devices whenever the window is
  // shown or refocused — otherwise a mic plugged in after launch wouldn't
  // appear until relaunch.
  document.addEventListener("visibilitychange", () => {
    if (document.visibilityState === "visible") refreshMics();
  });
  window.addEventListener("focus", refreshMics);

  // Dev screenshot tooling: a debug build launched with MURMUR_UI_PANE sets
  // window.__LAUNCH_PANE via an initialization script (see show_main_window), so
  // scripts/ui-shot.mjs can capture any pane. Unset in normal use.
  if (window.__LAUNCH_PANE) {
    switchTab(window.__LAUNCH_PANE);
  } else {
    // Populate whichever tab is active on launch (switchTab handles this on
    // click, but the landing tab is set in markup and never goes through it).
    const landing = document.querySelector(".nav-item.active")?.dataset.p;
    if (landing === TABS.HISTORY) loadHistory();
    else if (landing === TABS.HOME) loadInsights();
  }
}

if (document.readyState === "loading") {
  window.addEventListener("DOMContentLoaded", init);
} else {
  init();
}
