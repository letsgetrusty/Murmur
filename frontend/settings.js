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
    el("save").disabled = true;
  } catch (e) {
    setStatus(`Load failed: ${e}`, "error");
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
};
const HOTKEY_LABEL = {
  dictate: "Dictation chord",
  tts_toggle: "Read aloud",
  tts_speed: "Cycle speed",
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

  row.append(head, purpose, controls, msg);
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
  el("u-refresh").addEventListener("click", loadUsage);
  el("u-reset").addEventListener("click", async () => {
    try {
      await invoke("reset_usage");
      localUsage = null;
      renderUsageAll();
      el("usage-msg").textContent = "Local counts reset (OpenRouter total is provider-side).";
    } catch (e) {
      el("usage-msg").textContent = `Reset failed: ${e}`;
    }
  });
  // Live-update the local counts while the window is open.
  window.__TAURI__?.event?.listen?.("usage", (e) => {
    localUsage = e.payload;
    renderUsageAll();
  });
}

// --- Tabs & init --------------------------------------------------------------

function switchTab(name) {
  for (const b of document.querySelectorAll(".nav-item")) {
    b.classList.toggle("active", b.dataset.tab === name);
  }
  for (const t of document.querySelectorAll(".tab")) {
    t.classList.toggle("active", t.id === `tab-${name}`);
  }
}

async function init() {
  for (const b of document.querySelectorAll(".nav-item")) {
    b.addEventListener("click", () => switchTab(b.dataset.tab));
  }
  el("refine-prompt").addEventListener("input", markDirty);
  el("refine-model").addEventListener("input", markDirty);
  el("save").addEventListener("click", save);
  window.addEventListener("keydown", (e) => {
    if (e.metaKey && e.key === "s" && !recording) {
      e.preventDefault();
      if (!el("save").disabled) save();
    }
  });

  await loadConfig();
  await loadOptions();
  initHotkeys();
  await loadKeys();
  initUsage();
  await loadUsage();
}

if (document.readyState === "loading") {
  window.addEventListener("DOMContentLoaded", init);
} else {
  init();
}
