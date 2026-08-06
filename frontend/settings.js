// Settings/history window. Talks to the Rust backend over Tauri IPC commands
// (the overlay is event-driven; this window is request/response).

const invoke = window.__TAURI__?.core?.invoke;

// The full config object, kept so Save round-trips fields this UI doesn't edit.
let currentConfig = null;

const el = (id) => document.getElementById(id);

// --- Refinement (Save-based) --------------------------------------------------

function setStatus(text, kind = "") {
  const s = el("status");
  s.textContent = text;
  s.className = `status ${kind}`;
}

function markDirty() {
  el("save").disabled = false;
  setStatus("");
}

async function loadConfig() {
  if (!invoke) {
    setStatus("IPC unavailable", "error");
    return;
  }
  try {
    currentConfig = await invoke("get_config");
    el("refine-prompt").value = currentConfig.refine_prompt ?? "";
    el("refine-model").value = currentConfig.refine_model ?? "";
    el("stt-provider").value = currentConfig.stt_provider ?? "local";
    el("stt-model").value = currentConfig.stt_model ?? "small.en";
    el("tts-provider").value = currentConfig.tts_provider ?? "native";
    el("llm-provider").value = currentConfig.llm_provider ?? "local";
    el("save").disabled = true;
  } catch (e) {
    setStatus(`Load failed: ${e}`, "error");
  }
}

// Engine (STT/TTS) selections save immediately and prompt a relaunch, since the
// backends are built once at startup — a plain config save wouldn't swap them.
async function saveEngines() {
  if (!invoke || !currentConfig) return;
  const next = {
    ...currentConfig,
    stt_provider: el("stt-provider").value,
    stt_model: el("stt-model").value,
    tts_provider: el("tts-provider").value,
    llm_provider: el("llm-provider").value,
  };
  try {
    await invoke("save_config", { config: next });
    currentConfig = next;
    el("engine-relaunch").hidden = false;
  } catch (e) {
    setStatus(`Save failed: ${e}`, "error");
  }
}

async function save() {
  if (!invoke || !currentConfig) return;
  const next = {
    ...currentConfig,
    refine_prompt: el("refine-prompt").value,
    refine_model: el("refine-model").value.trim(),
  };
  el("save").disabled = true;
  setStatus("Saving…");
  try {
    await invoke("save_config", { config: next });
    currentConfig = next;
    setStatus("Saved ✓", "ok");
  } catch (e) {
    setStatus(`Save failed: ${e}`, "error");
    el("save").disabled = false;
  }
}

// --- Audio (apply immediately) ------------------------------------------------

function addOption(select, value, label) {
  const o = document.createElement("option");
  o.value = value;
  o.textContent = label;
  select.appendChild(o);
}

async function loadOptions() {
  if (!invoke || !currentConfig) return;
  let opts;
  try {
    opts = await invoke("get_options");
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

  const mic = el("mic");
  mic.innerHTML = "";
  addOption(mic, "", "System default");
  for (const m of opts.mics) addOption(mic, m, m);
  mic.value = currentConfig.mic_name ?? "";

  speed.addEventListener("change", (e) =>
    invoke("set_speed", { speed: parseFloat(e.target.value) })
  );
  voice.addEventListener("change", (e) =>
    invoke("set_voice", { voiceId: e.target.value })
  );
  mic.addEventListener("change", (e) => {
    const v = e.target.value;
    invoke("set_mic", { name: v === "" ? null : v });
  });
}

// --- Keyboard shortcuts -------------------------------------------------------

const HOTKEY_FIELD = {
  dictate: "hotkey_dictate",
  tts_toggle: "hotkey_tts",
  tts_speed: "hotkey_tts_speed",
  macro: "hotkey_macro",
};
const HOTKEY_LABEL = {
  dictate: "Dictation chord",
  tts_toggle: "Read aloud",
  tts_speed: "Cycle speed",
  macro: "Macro chord",
};

function prettyShortcut(s) {
  return (s || "")
    .split("+")
    .map((t) => {
      switch (t) {
        case "CmdOrCtrl":
        case "Cmd":
        case "Command":
        case "Super":
          return "⌘";
        case "Ctrl":
        case "Control":
          return "⌃";
        case "Alt":
        case "Option":
          return "⌥";
        case "Shift":
          return "⇧";
        default:
          return t;
      }
    })
    .join("");
}

function codeToKey(code) {
  if (/^Key[A-Z]$/.test(code)) return code.slice(3);
  if (/^Digit[0-9]$/.test(code)) return code.slice(5);
  if (/^F([1-9]|1[0-9]|2[0-4])$/.test(code)) return code;
  const map = {
    Space: "Space",
    Enter: "Enter",
    Tab: "Tab",
    Backspace: "Backspace",
    Delete: "Delete",
    ArrowUp: "Up",
    ArrowDown: "Down",
    ArrowLeft: "Left",
    ArrowRight: "Right",
    Minus: "-",
    Equal: "=",
    BracketLeft: "[",
    BracketRight: "]",
    Backslash: "\\",
    Semicolon: ";",
    Quote: "'",
    Comma: ",",
    Period: ".",
    Slash: "/",
    Backquote: "`",
  };
  return map[code] || null; // pure modifier codes fall through to null
}

function setHotkeyStatus(text, kind = "") {
  const s = el("hotkey-status");
  s.textContent = text;
  s.className = `hint ${kind}`;
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
  recording.button.classList.remove("recording");
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
    stopRecording(true);
    setHotkeyStatus("Cancelled.");
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
    setHotkeyStatus("Add at least one modifier (⌘/⌃/⌥/⇧).", "error");
    return;
  }

  const shortcut = [...mods, key].join("+");
  const { action, button } = recording;
  stopRecording(false);
  button.textContent = prettyShortcut(shortcut);
  try {
    await invoke("set_hotkey", { action, shortcut });
    currentConfig[HOTKEY_FIELD[action]] = shortcut;
    setHotkeyStatus(`${HOTKEY_LABEL[action]} → ${prettyShortcut(shortcut)} ✓`, "ok");
  } catch (err) {
    button.textContent = prettyShortcut(currentConfig[HOTKEY_FIELD[action]]);
    setHotkeyStatus(`Couldn't set that: ${err}`, "error");
  }
}

function initHotkeys() {
  renderHotkeys();
  for (const btn of document.querySelectorAll(".recorder")) {
    btn.addEventListener("click", () => {
      if (recording) stopRecording(true);
      recording = { action: btn.dataset.action, button: btn };
      btn.classList.add("recording");
      btn.textContent = "Press keys…";
      window.addEventListener("keydown", onRecordKey, true);
    });
  }

  // Refined dictation: Fn + a configurable modifier.
  const rm = el("refine-modifier");
  if (rm) {
    rm.value = currentConfig.refine_modifier ?? "Ctrl";
    rm.addEventListener("change", async (e) => {
      const modifier = e.target.value;
      try {
        await invoke("set_refine_modifier", { modifier });
        currentConfig.refine_modifier = modifier;
        setHotkeyStatus(`Refined dictation → Fn+${modifier} ✓`, "ok");
      } catch (err) {
        rm.value = currentConfig.refine_modifier ?? "Ctrl";
        setHotkeyStatus(`Couldn't set that: ${err}`, "error");
      }
    });
  }
}

// --- API keys -----------------------------------------------------------------

function setKeyMsg(row, text, kind = "") {
  const m = row.querySelector(".key-msg");
  if (m) {
    m.textContent = text;
    m.className = `key-msg hint ${kind}`;
  }
}

// Where to obtain each provider's API key, keyed by the id from list_keys.
const KEY_SIGNUP_URLS = {
  groq: "https://console.groq.com/keys",
  openrouter: "https://openrouter.ai/keys",
  elevenlabs: "https://elevenlabs.io/app/settings/api-keys",
};

function renderKeyRow(k) {
  const row = document.createElement("div");
  row.className = "key-row";

  const head = document.createElement("div");
  head.className = "key-head";
  const label = document.createElement("span");
  label.className = "row-label";
  label.textContent = k.label;
  const status = document.createElement("span");
  status.className = `key-status ${k.present ? "ok" : "off"}`;
  status.textContent = k.present ? `Set · ${k.masked}` : "Not set";
  head.append(label, status);

  const purpose = document.createElement("div");
  purpose.className = "hint key-purpose";
  purpose.textContent = k.purpose;

  const controls = document.createElement("div");
  controls.className = "key-controls";

  const input = document.createElement("input");
  input.type = "password";
  input.className = "key-input";
  input.autocomplete = "off";
  input.spellcheck = false;
  input.placeholder = k.present ? "•••• set — paste to replace" : "paste key…";

  const reveal = document.createElement("button");
  reveal.className = "small-btn";
  reveal.textContent = "Reveal";
  reveal.disabled = !k.present;
  let revealed = false;
  reveal.addEventListener("click", async () => {
    if (revealed) {
      input.type = "password";
      input.value = "";
      reveal.textContent = "Reveal";
      revealed = false;
      return;
    }
    try {
      input.value = await invoke("reveal_key", { id: k.id });
      input.type = "text";
      reveal.textContent = "Hide";
      revealed = true;
    } catch (e) {
      setKeyMsg(row, `Reveal failed: ${e}`, "error");
    }
  });

  const save = document.createElement("button");
  save.className = "primary small";
  save.textContent = "Save";
  save.addEventListener("click", async () => {
    const v = input.value.trim();
    if (!v) {
      setKeyMsg(row, "Enter a key first.", "error");
      return;
    }
    try {
      await invoke("save_key", { id: k.id, value: v });
      setKeyMsg(row, "Saved ✓ — relaunch to apply.", "ok");
      await loadKeys();
    } catch (e) {
      setKeyMsg(row, `Save failed: ${e}`, "error");
    }
  });

  const remove = document.createElement("button");
  remove.className = "small-btn danger";
  remove.textContent = "Remove";
  remove.disabled = !k.present;
  remove.addEventListener("click", async () => {
    try {
      await invoke("delete_key", { id: k.id });
      await loadKeys();
    } catch (e) {
      setKeyMsg(row, `Remove failed: ${e}`, "error");
    }
  });

  controls.append(input, reveal, save, remove);

  const msg = document.createElement("div");
  msg.className = "key-msg hint";

  row.append(head, purpose, controls);

  // "Get a key" link → opens the provider's key page in the default browser.
  const url = KEY_SIGNUP_URLS[k.id];
  if (url) {
    const link = document.createElement("a");
    link.className = "key-link";
    link.href = url;
    link.textContent = `Get a ${k.label} API key ↗`;
    link.addEventListener("click", (e) => {
      e.preventDefault();
      if (invoke) invoke("open_url", { url }).catch(() => {});
    });
    row.append(link);
  }

  row.append(msg);
  return row;
}

async function loadKeys() {
  if (!invoke) return;
  let keys;
  try {
    keys = await invoke("list_keys");
  } catch (e) {
    return;
  }
  const container = el("keys");
  container.innerHTML = "";
  for (const k of keys) container.appendChild(renderKeyRow(k));
}

// --- Usage --------------------------------------------------------------------

// Groq whisper-large-v3-turbo list price (per hour of audio) for the estimate.
const GROQ_USD_PER_SEC = 0.04 / 3600;

let localUsage = null; // from get_usage (STT/TTS counts, refine tokens)
let orSpend = null; // from get_openrouter_spend (real $), or null on error

function fmtCost(usd) {
  const n = usd || 0;
  return n < 1 ? `$${n.toFixed(4)}` : `$${n.toFixed(2)}`;
}

function renderUsageAll() {
  const u = localUsage || {};

  // Refinement · OpenRouter — cost is exact from the API; tokens/count local.
  el("or-total").textContent = orSpend ? fmtCost(orSpend.total_usd) : "n/a";
  el("or-sub").textContent = [
    orSpend ? `${fmtCost(orSpend.month_usd)} this month` : null,
    `${(u.total_tokens || 0).toLocaleString()} tokens`,
    `${u.refine_count || 0} refinements`,
  ]
    .filter(Boolean)
    .join(" · ");

  // Dictation · Groq — estimated from audio duration.
  const groqCost = (u.stt_seconds || 0) * GROQ_USD_PER_SEC;
  el("stt-cost").textContent = `~${fmtCost(groqCost)}`;
  el("stt-sub").textContent = `${u.stt_count || 0} clips · ${((u.stt_seconds || 0) / 60).toFixed(
    1
  )} min · est. @ $0.04/hr`;

  // Read-aloud · ElevenLabs — characters (cost varies by plan).
  el("tts-cost").textContent = `${(u.tts_chars || 0).toLocaleString()} chars`;
  el("tts-sub").textContent = `${u.tts_count || 0} reads · $ varies by plan`;
}

async function fetchOpenRouterSpend() {
  try {
    orSpend = await invoke("get_openrouter_spend");
    el("usage-msg").textContent = "";
  } catch (e) {
    orSpend = null;
    el("usage-msg").textContent = `OpenRouter usage unavailable: ${e}`;
  }
  renderUsageAll();
}

async function loadUsage() {
  if (!invoke) return;
  try {
    localUsage = await invoke("get_usage");
  } catch (e) {
    /* ignore */
  }
  renderUsageAll();
  fetchOpenRouterSpend();
}

function initUsage() {
  el("u-refresh").addEventListener("click", loadInsights);
  el("u-reset").addEventListener("click", async () => {
    try {
      await invoke("reset_usage");
      localUsage = null;
      renderUsageAll();
      renderInsights();
      el("usage-reset-msg").textContent = "Local counts reset (OpenRouter total is provider-side).";
    } catch (e) {
      el("usage-reset-msg").textContent = `Reset failed: ${e}`;
    }
  });
  // Live-update the counts while the window is open.
  window.__TAURI__?.event?.listen?.("usage", (e) => {
    localUsage = e.payload;
    renderUsageAll();
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
    insStats = await invoke("history_stats");
  } catch (_) {
    insStats = null;
  }
  await loadUsage(); // refreshes the provider cards + OpenRouter spend
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
      await invoke("copy_text", { text });
      copy.textContent = "Copied";
      setTimeout(() => (copy.textContent = "Copy"), 1200);
    } catch (_) {}
  });
  const del = document.createElement("button");
  del.className = "small-btn danger";
  del.textContent = "Delete";
  del.addEventListener("click", async () => {
    try {
      await invoke("delete_history", { id: e.id });
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
    const entries = await invoke("list_history", {
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
  const enabled = el("history-enabled");
  enabled.checked = currentConfig?.history_enabled ?? true;
  enabled.addEventListener("change", async (e) => {
    const next = { ...currentConfig, history_enabled: e.target.checked };
    try {
      await invoke("save_config", { config: next });
      currentConfig = next;
    } catch (_) {}
  });

  let t = null;
  el("history-search").addEventListener("input", (e) => {
    historyQuery = e.target.value;
    clearTimeout(t);
    t = setTimeout(loadHistory, 150);
  });

  el("history-clear").addEventListener("click", async () => {
    if (!window.confirm("Delete all dictation history?")) return;
    try {
      await invoke("clear_history");
      loadHistory();
    } catch (_) {}
  });

  // Refresh live when a new dictation lands and we're on the relevant tab.
  window.__TAURI__?.event?.listen?.("history", () => {
    if (document.getElementById("tab-history")?.classList.contains("active")) loadHistory();
    if (document.getElementById("tab-insights")?.classList.contains("active")) loadInsights();
  });
}

// --- Macros -------------------------------------------------------------------
// Edited in-memory as DOM rows, then round-tripped through save_config (same
// whole-config save the rest of Settings uses). Kept stable across tab switches
// so unsaved edits survive navigation — only rebuilt on load.

function setMacroStatus(text, kind = "") {
  const s = el("macro-status");
  s.textContent = text;
  s.className = `status ${kind}`;
}

function markMacrosDirty() {
  el("macro-save").disabled = false;
  setMacroStatus("");
}

function macroRow(m = { name: "", triggers: "", response: "" }) {
  const row = document.createElement("div");
  row.className = "macro-row";

  const head = document.createElement("div");
  head.className = "macro-head";
  const name = document.createElement("input");
  name.className = "macro-name";
  name.placeholder = "Name — e.g. Schedule call";
  name.value = m.name ?? "";
  const remove = document.createElement("button");
  remove.className = "small-btn danger";
  remove.textContent = "Remove";
  remove.addEventListener("click", () => {
    row.remove();
    markMacrosDirty();
  });
  head.append(name, remove);

  const triggers = document.createElement("input");
  triggers.className = "macro-triggers";
  triggers.placeholder = "Example phrases (optional) — e.g. book a call, send my calendly";
  triggers.value = m.triggers ?? "";

  const response = document.createElement("textarea");
  response.className = "macro-response";
  response.rows = 3;
  response.spellcheck = false;
  response.placeholder = "Text to paste — e.g. https://calendly.com/you/30min";
  response.value = m.response ?? "";

  for (const inp of [name, triggers, response]) inp.addEventListener("input", markMacrosDirty);

  row.append(head, triggers, response);
  return row;
}

function renderMacros() {
  const list = el("macro-list");
  list.innerHTML = "";
  for (const m of currentConfig?.macros ?? []) list.appendChild(macroRow(m));
}

// Read the macro rows back out of the DOM. Fully-empty rows are dropped.
function collectMacros() {
  const macros = [];
  for (const r of el("macro-list").querySelectorAll(".macro-row")) {
    const name = r.querySelector(".macro-name").value.trim();
    const triggers = r.querySelector(".macro-triggers").value.trim();
    const response = r.querySelector(".macro-response").value;
    if (!name && !triggers && !response.trim()) continue;
    macros.push({ name, triggers, response });
  }
  return macros;
}

async function saveMacros() {
  if (!invoke || !currentConfig) return;
  const macros = collectMacros();
  for (const m of macros) {
    if (!m.name || !m.response.trim()) {
      setMacroStatus("Each macro needs a name and a response.", "error");
      return;
    }
  }
  const next = { ...currentConfig, macros, macro_model: el("macro-model").value.trim() };
  el("macro-save").disabled = true;
  setMacroStatus("Saving…");
  try {
    await invoke("save_config", { config: next });
    currentConfig = next;
    setMacroStatus("Saved ✓", "ok");
  } catch (e) {
    setMacroStatus(`Save failed: ${e}`, "error");
    el("macro-save").disabled = false;
  }
}

function initMacros() {
  el("macro-model").value = currentConfig?.macro_model ?? "";
  renderMacros();
  el("macro-model").addEventListener("input", markMacrosDirty);
  el("macro-add").addEventListener("click", () => {
    el("macro-list").appendChild(macroRow());
    markMacrosDirty();
  });
  el("macro-save").addEventListener("click", saveMacros);
}

// --- Tabs & init --------------------------------------------------------------

function switchTab(name) {
  for (const b of document.querySelectorAll(".nav-item")) {
    b.classList.toggle("active", b.dataset.tab === name);
  }
  for (const t of document.querySelectorAll(".tab")) {
    t.classList.toggle("active", t.id === `tab-${name}`);
  }
  if (name === "history") loadHistory();
  if (name === "insights") loadInsights();
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

async function init() {
  initZoom();
  for (const b of document.querySelectorAll(".nav-item")) {
    b.addEventListener("click", () => switchTab(b.dataset.tab));
  }
  el("refine-prompt").addEventListener("input", markDirty);
  el("refine-model").addEventListener("input", markDirty);
  for (const id of ["stt-provider", "stt-model", "tts-provider", "llm-provider"]) {
    el(id).addEventListener("change", saveEngines);
  }
  el("engine-relaunch-btn").addEventListener("click", () => invoke?.("relaunch_app"));
  el("save").addEventListener("click", save);
  window.addEventListener("keydown", (e) => {
    if (e.metaKey && e.key === "s" && !recording) {
      e.preventDefault();
      const onMacros = document.getElementById("tab-macros")?.classList.contains("active");
      if (onMacros) {
        if (!el("macro-save").disabled) saveMacros();
      } else if (!el("save").disabled) {
        save();
      }
    }
  });

  await loadConfig();
  await loadOptions();
  initHotkeys();
  initMacros();
  await loadKeys();
  initUsage();
  initHistory();
}

if (document.readyState === "loading") {
  window.addEventListener("DOMContentLoaded", init);
} else {
  init();
}
