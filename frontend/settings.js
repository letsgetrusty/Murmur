// Settings/history window. Talks to the Rust backend over Tauri IPC commands
// (the overlay is event-driven; this window is request/response).

const invoke = window.__TAURI__?.core?.invoke;

// The full config object, kept so Save round-trips fields this UI doesn't edit
// (tts, mic, history) without clobbering them.
let currentConfig = null;

const el = (id) => document.getElementById(id);

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

function switchTab(name) {
  for (const b of document.querySelectorAll(".nav-item")) {
    b.classList.toggle("active", b.dataset.tab === name);
  }
  for (const t of document.querySelectorAll(".tab")) {
    t.classList.toggle("active", t.id === `tab-${name}`);
  }
}

function init() {
  for (const b of document.querySelectorAll(".nav-item")) {
    b.addEventListener("click", () => switchTab(b.dataset.tab));
  }
  el("refine-prompt").addEventListener("input", markDirty);
  el("refine-model").addEventListener("input", markDirty);
  el("save").addEventListener("click", save);
  // Cmd+S saves.
  window.addEventListener("keydown", (e) => {
    if (e.metaKey && e.key === "s") {
      e.preventDefault();
      if (!el("save").disabled) save();
    }
  });
  loadConfig();
}

if (document.readyState === "loading") {
  window.addEventListener("DOMContentLoaded", init);
} else {
  init();
}
