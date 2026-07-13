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
}

if (document.readyState === "loading") {
  window.addEventListener("DOMContentLoaded", init);
} else {
  init();
}
