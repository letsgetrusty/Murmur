// Settings/history window. Talks to the Rust backend over Tauri IPC commands
// (the overlay is event-driven; this window is request/response).

import { EVENTS, CMD, TABS } from "./constants.js";
import { prettyShortcut, codeToKey } from "./shortcuts.js";

const invoke = window.__TAURI__?.core?.invoke;

// The full config object, kept so Save round-trips fields this UI doesn't edit.
let currentConfig = null;

const el = (id) => document.getElementById(id);

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

function addOption(select, value, label) {
  const o = document.createElement("option");
  o.value = value;
  o.textContent = label;
  select.appendChild(o);
}

// (Re)build the microphone dropdown from the current input-device list.
function fillMics(mics) {
  const mic = el("mic");
  const current = currentConfig?.mic_name ?? "";
  mic.innerHTML = "";
  addOption(mic, "", "System default");
  for (const m of mics) addOption(mic, m, m);
  // Keep the saved selection if the device is still present; otherwise show
  // System default (the recorder falls back to the default the same way).
  mic.value = mics.includes(current) ? current : "";
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

  const speed = el("speed");
  speed.innerHTML = "";
  for (const s of opts.speeds) addOption(speed, String(s), `${s.toFixed(1)}×`);
  speed.value = String(currentConfig.tts_speed);

  const voice = el("voice");
  voice.innerHTML = "";
  for (const v of opts.voices) addOption(voice, v.id, v.name);
  voice.value = currentConfig.tts_voice_id;

  fillMics(opts.mics);

  speed.addEventListener("change", (e) =>
    invoke(CMD.SET_SPEED, { speed: parseFloat(e.target.value) })
  );
  voice.addEventListener("change", async (e) => {
    await invoke(CMD.SET_VOICE, { voiceId: e.target.value });
    // Preview the new voice: "Hey, my name is <Name>!". The option label is the
    // friendly name (e.g. "Heart (US female)"); take the part before " (".
    const label = e.target.selectedOptions[0]?.textContent ?? "";
    const name = label.split("(")[0].trim();
    invoke(CMD.PREVIEW_VOICE, { name });
  });
  mic.addEventListener("change", (e) => {
    const v = e.target.value;
    invoke(CMD.SET_MIC, { name: v === "" ? null : v });
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
      el("speed").value = String(cfg.tts_speed);
      el("voice").value = cfg.tts_voice_id;
      el("mic").value = cfg.mic_name ?? "";
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

// Combos we never let a global shortcut take: core macOS editing keys and the
// ones Murmur synthesizes for paste/copy — binding those would break dictation.
// Mirrors is_reserved_shortcut() in hotkeys.rs (the backend rejects them too).
const RESERVED_SHORTCUTS = new Set([
  "Cmd+V",
  "Cmd+C",
  "Cmd+X",
  "Cmd+A",
  "Cmd+Z",
  "Cmd+Q",
  "Cmd+W",
]);

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

let recording = null;

function renderHotkeys() {
  for (const btn of document.querySelectorAll(".recorder")) {
    btn.textContent = prettyShortcut(currentConfig[HOTKEY_FIELD[btn.dataset.action]]);
  }
}

function stopRecording(restore) {
  if (!recording) return;
  window.removeEventListener("keydown", onRecordKey, true);
  recording.button.classList.remove("recording", "invalid");
  if (restore) {
    recording.button.textContent = prettyShortcut(
      currentConfig[HOTKEY_FIELD[recording.action]]
    );
  }
  recording = null;
}

async function onRecordKey(e) {
  e.preventDefault();
  e.stopPropagation();
  if (e.key === "Escape") {
    stopRecording(true); // reverting the label is the cancel confirmation
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
    // Inline validation on the active recorder; keep listening so the user can
    // immediately retry with a modifier held.
    recording.button.classList.add("invalid");
    recording.button.textContent = "Hold a modifier ⌘⌃⌥⇧";
    return;
  }

  const shortcut = [...mods, key].join("+");
  // Refuse core-editing / app-synthesized combos (Cmd+V, Cmd+C, …). Binding one
  // globally would swallow the app's own paste and silently break dictation.
  // Keep listening so the user can pick something else.
  if (RESERVED_SHORTCUTS.has(shortcut)) {
    recording.button.classList.add("invalid");
    recording.button.textContent = "Reserved — pick another";
    return;
  }
  const { action, button } = recording;
  stopRecording(false);
  button.textContent = prettyShortcut(shortcut); // label change confirms success
  try {
    await invoke(CMD.SET_HOTKEY, { action, shortcut });
    currentConfig[HOTKEY_FIELD[action]] = shortcut;
  } catch (err) {
    button.textContent = prettyShortcut(currentConfig[HOTKEY_FIELD[action]]);
    flashInvalid(button);
  }
}

function initHotkeys() {
  renderHotkeys();
  for (const btn of document.querySelectorAll(".recorder")) {
    btn.addEventListener("click", () => {
      if (recording) stopRecording(true);
      recording = { action: btn.dataset.action, button: btn };
      btn.classList.remove("invalid");
      btn.classList.add("recording");
      btn.textContent = "Press keys…";
      window.addEventListener("keydown", onRecordKey, true);
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
      if (recording) stopRecording(true);
      try {
        currentConfig = await invoke(CMD.RESET_HOTKEYS);
        renderHotkeys();
        if (dt) dt.value = currentConfig.dictation_trigger ?? "Fn";
        if (rm) rm.value = currentConfig.refine_modifier ?? "Ctrl";
        syncTriggerBadge();
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

// --- Insights -----------------------------------------------------------------

let insStats = null; // last history_stats { total, refined, words, first_ts, days[] }

function fmtDuration(sec) {
  sec = Math.round(sec || 0);
  if (sec < 60) return `${sec}s`;
  const m = Math.round(sec / 60);
  if (m < 60) return `${m} min`;
  return `${Math.floor(m / 60)}h ${m % 60}m`;
}

function renderInsights() {
  const u = localUsage || {};
  const s = insStats || { total: 0, refined: 0, words: 0, days: [] };

  const dictations = u.stt_count || 0;
  el("ins-dictations").textContent = dictations.toLocaleString();
  el("ins-dictations-sub").textContent = (u.refine_count || 0)
    ? `${(u.refine_count || 0).toLocaleString()} refined`
    : "";

  el("ins-words").textContent = (s.words || 0).toLocaleString();
  el("ins-words-sub").textContent = s.total ? `~${Math.round(s.words / s.total)} per dictation` : "";

  el("ins-time").textContent = fmtDuration(u.stt_seconds);
  el("ins-time-sub").textContent = dictations
    ? `~${Math.round((u.stt_seconds || 0) / dictations)}s each`
    : "";

  el("ins-reads").textContent = (u.tts_count || 0).toLocaleString();
  el("ins-reads-sub").textContent = (u.tts_chars || 0)
    ? `${(u.tts_chars || 0).toLocaleString()} chars`
    : "";

  renderActivity(s);
}

function renderActivity(s) {
  const host = el("ins-activity");
  const note = el("ins-activity-note");
  host.innerHTML = "";
  const days = s.days || [];
  const inWindow = days.reduce((a, d) => a + d.count, 0);

  if (!days.length || inWindow === 0) {
    // History off (lifetime dictations exist but nothing stored) vs simply quiet.
    const off = (localUsage?.stt_count || 0) > 0 && (s.total || 0) === 0;
    host.innerHTML = `<span class="act-empty">${
      off ? "Turn on History recording to track activity." : "No dictations in the last 14 days."
    }</span>`;
    note.textContent = off ? "History off" : "Last 14 days";
    return;
  }

  note.textContent = "Last 14 days";
  const max = Math.max(...days.map((d) => d.count), 1);
  days.forEach((d, i) => {
    const col = document.createElement("div");
    col.className = "act-col";
    col.title = `${d.date}: ${d.count} dictation${d.count === 1 ? "" : "s"}`;
    const bar = document.createElement("div");
    bar.className = "act-bar" + (i === days.length - 1 ? " today" : "");
    bar.style.height = `${Math.max(4, (d.count / max) * 100)}%`;
    col.appendChild(bar);
    host.appendChild(col);
  });
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

function renderHistoryRow(e) {
  const text = e.refined ?? e.raw;
  const row = document.createElement("div");
  row.className = "hist-row";

  const head = document.createElement("div");
  head.className = "hist-head";
  const badge = document.createElement("span");
  badge.className = `hist-badge ${e.refined ? "refined" : "raw"}`;
  badge.textContent = e.refined ? "Refined" : "Raw";
  const time = document.createElement("span");
  time.className = "hist-time";
  time.textContent = fmtTime(e.ts);
  const spacer = document.createElement("span");
  spacer.className = "hist-spacer";

  const copy = document.createElement("button");
  copy.className = "small-btn";
  copy.textContent = "Copy";
  copy.addEventListener("click", async () => {
    try {
      await invoke(CMD.COPY_TEXT, { text });
      copy.textContent = "Copied";
      setTimeout(() => (copy.textContent = "Copy"), 1200);
    } catch (_) {}
  });
  const del = document.createElement("button");
  del.className = "small-btn danger";
  del.textContent = "Delete";
  del.addEventListener("click", async () => {
    try {
      await invoke(CMD.DELETE_HISTORY, { id: e.id });
      row.remove();
      if (!el("history-list").children.length) el("history-empty").hidden = false;
    } catch (_) {}
  });
  head.append(badge, time, spacer, copy, del);

  const body = document.createElement("div");
  body.className = "hist-text";
  body.textContent = text;

  row.append(head, body);
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
    if (document.getElementById(`tab-${TABS.INSIGHTS}`)?.classList.contains("active")) loadInsights();
  });
}

// --- Support ------------------------------------------------------------------

const REPO_URL = "https://github.com/letsgetrusty/Murmur";
const ISSUES_URL = `${REPO_URL}/issues`;

async function loadSupport() {
  if (!invoke) return;
  try {
    const version = await invoke(CMD.APP_VERSION);
    el("app-version").textContent = `v${version}`;
  } catch (_) {
    el("app-version").textContent = "unknown";
  }
}

function initSupport() {
  const open = (url) => invoke?.(CMD.OPEN_URL, { url }).catch(() => {});
  el("report-issue")?.addEventListener("click", () => open(ISSUES_URL));
  el("view-github")?.addEventListener("click", () => open(REPO_URL));
}

// --- Tabs & init --------------------------------------------------------------

function switchTab(name) {
  for (const b of document.querySelectorAll(".nav-item")) {
    b.classList.toggle("active", b.dataset.tab === name);
  }
  for (const t of document.querySelectorAll(".tab")) {
    t.classList.toggle("active", t.id === `tab-${name}`);
  }
  if (name === TABS.HISTORY) loadHistory();
  if (name === TABS.INSIGHTS) loadInsights();
}

// --- UI zoom (Cmd +/-/0) ------------------------------------------------------
// WKWebView ignores Tauri's native zoom hotkeys, so scale the page with CSS
// `zoom` and persist the level (survives the window being recreated on reopen).
const ZOOM_KEY = "ui-zoom";
let uiZoom = parseFloat(localStorage.getItem(ZOOM_KEY)) || 1;

function applyZoom() {
  uiZoom = Math.round(Math.min(Math.max(uiZoom, 0.6), 2.5) * 100) / 100;
  document.documentElement.style.zoom = String(uiZoom);
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

// A newer version has been downloaded + verified in the background and is
// staged — the user just needs to restart to apply it.
function showUpdateStaged(version) {
  const banner = el("update-banner");
  banner.classList.remove("neutral");
  el("update-text").innerHTML = `<b>Murmur ${version}</b> is downloaded and ready to install.`;
  const btn = el("update-install");
  btn.hidden = false;
  btn.disabled = false;
  btn.textContent = "Restart to update";
  banner.hidden = false;
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

function initUpdate() {
  el("update-install").addEventListener("click", installUpdate);
  const listen = window.__TAURI__?.event?.listen;
  listen?.(EVENTS.UPDATE_STAGED, (e) => showUpdateStaged(e.payload));
  listen?.(EVENTS.UPDATE_NONE, () => showUpToDate());
  // If an update was already staged before this window opened, show it.
  invoke?.(CMD.PENDING_UPDATE_VERSION).then((v) => {
    if (v) showUpdateStaged(v);
  });
}

async function init() {
  initZoom();
  for (const b of document.querySelectorAll(".nav-item")) {
    b.addEventListener("click", () => switchTab(b.dataset.tab));
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

  // Populate whichever tab is active on launch (switchTab handles this on click,
  // but the landing tab is set in markup and never goes through it).
  const landing = document.querySelector(".nav-item.active")?.dataset.tab;
  if (landing === TABS.HISTORY) loadHistory();
  else if (landing === TABS.INSIGHTS) loadInsights();
}

if (document.readyState === "loading") {
  window.addEventListener("DOMContentLoaded", init);
} else {
  init();
}
