// Shared string constants for the webviews (overlay, settings, onboarding).
// Centralizes the "magic strings" that must match the Rust backend — Tauri
// event names and command names — or are referenced across files, so a typo
// surfaces as a build/import error instead of a silent no-op.

// Tauri events emitted by the Rust backend. Keep in sync with the `emit(...)`
// calls in src-tauri/src/lib.rs (mirrored there as the `events` module).
export const EVENTS = {
  STATE: "state", // overlay: recording/transcribing/… state changes
  AUDIO_LEVEL: "audio:level", // overlay: mic level for the waveform
  USAGE: "usage", // settings: usage counters changed
  HISTORY: "history", // settings: a dictation was recorded/cleared
  MODEL_DOWNLOAD: "model-download", // onboarding: model download progress
  UPDATE_STAGED: "update-staged", // settings: an update finished downloading
  UPDATE_NONE: "update-none", // settings: check found no update
};

// Tauri commands. Must match the #[tauri::command] fn names in src-tauri/src.
export const CMD = {
  GET_CONFIG: "get_config",
  SAVE_CONFIG: "save_config",
  GET_OPTIONS: "get_options",
  SET_SPEED: "set_speed",
  SET_VOICE: "set_voice",
  SET_MIC: "set_mic",
  SET_HOTKEY: "set_hotkey",
  SET_REFINE_MODIFIER: "set_refine_modifier",
  SET_NEURAL_VOICE: "set_neural_voice",
  GET_USAGE: "get_usage",
  HISTORY_STATS: "history_stats",
  LIST_HISTORY: "list_history",
  DELETE_HISTORY: "delete_history",
  CLEAR_HISTORY: "clear_history",
  COPY_TEXT: "copy_text",
  RELAUNCH_APP: "relaunch_app",
  PENDING_UPDATE_VERSION: "pending_update_version",
  INSTALL_STAGED_UPDATE: "install_staged_update",
  ONBOARDING_STATUS: "onboarding_status",
  FINISH_ONBOARDING: "finish_onboarding",
  REQUEST_MICROPHONE: "request_microphone",
  OPEN_ACCESSIBILITY_SETTINGS: "open_accessibility_settings",
  OPEN_MICROPHONE_SETTINGS: "open_microphone_settings",
};

// Settings-window tabs: the value is both the nav `data-tab` and the
// `#tab-<value>` section id.
export const TABS = {
  HISTORY: "history",
  INSIGHTS: "insights",
  SETTINGS: "settings",
};

// Model-download ids, carried in the MODEL_DOWNLOAD payload's `id` field.
// Keep in sync with emit_download_progress(...) in src-tauri/src/lib.rs.
export const DOWNLOAD = {
  WHISPER: "whisper",
  LLM: "llm",
};
