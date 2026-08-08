//! String identifiers shared with the webviews. Mirrored on the frontend in
//! `frontend/constants.js` (EVENTS / DOWNLOAD) — keep the two in sync so a
//! rename can't silently break the Rust↔JS wiring.

/// Tauri event names emitted to the webviews (constants.js `EVENTS`).
pub mod event {
    /// Overlay: recording → transcribing → done/error state transitions.
    pub const STATE: &str = "state";
    /// Overlay: mic input level for the waveform.
    pub const AUDIO_LEVEL: &str = "audio:level";
    /// Settings: usage counters changed.
    pub const USAGE: &str = "usage";
    /// Settings: a dictation was recorded or history cleared.
    pub const HISTORY: &str = "history";
    /// Onboarding: model download progress.
    pub const MODEL_DOWNLOAD: &str = "model-download";
    /// Settings: an update finished downloading and is staged.
    pub const UPDATE_STAGED: &str = "update-staged";
    /// Settings: an update check found nothing newer.
    pub const UPDATE_NONE: &str = "update-none";
}

/// Model-download ids carried in the MODEL_DOWNLOAD payload's `id` field
/// (constants.js `DOWNLOAD`).
pub mod download {
    pub const WHISPER: &str = "whisper";
    pub const LLM: &str = "llm";
}
